use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use aes::Aes256;
use axum::{
    extract::{Path as AxumPath, Query, State as AxumState},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use feishu_sdk::{
    card::{CardAction, CardActionHandler},
    core::{noop_logger, Config, FEISHU_BASE_URL},
    event::{
        Event, EventDispatcher, EventDispatcherConfig, EventHandler, EventHandlerResult, EventResp,
    },
};
use futures_util::{SinkExt, StreamExt};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::{Client, StatusCode as HttpStatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::{Digest as Sha1Digest, Sha1};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

use crate::{
    database::{Repository, StoredCodexSession, StoredCollaborationBot},
    domain::{
        CancelCodexSessionInput, CodexAuthMode, CodexSessionEvent, CodexSessionSummary,
        CollaborationCallbackStatus, CollaborationCommandResult, CollaborationProjectBinding,
        CollaborationProvider, ContinueCodexSessionInput, DeleteCollaborationBotInput,
        DeleteCollaborationProjectBindingInput, ListCodexSessionsInput, MaskedCollaborationBot,
        ProfileKind, UpsertCollaborationBotInput, UpsertCollaborationProjectBindingInput,
    },
    error::{AppError, AppResult},
    oauth_credentials::{CredentialAccess as OAuthCredentialAccess, OAuthCredentialStore},
    profiles::{CodexOAuthCredential, ImportedAuthFileCredential},
    secrets::SecretStore,
};

const GLOBAL_CONCURRENCY_LIMIT: i64 = 4;
const DEFAULT_PROJECT_CONCURRENCY_LIMIT: i64 = 2;
const CALLBACK_PORT: u16 = 53821;
const CODEX_SESSION_ARGS: [&str; 6] = [
    "exec",
    "--json",
    "--sandbox",
    "workspace-write",
    "--output-last-message",
    "__OUTPUT__",
];
const CODEX_RESUME_ARGS: [&str; 7] = [
    "exec",
    "resume",
    "--json",
    "--output-last-message",
    "__OUTPUT__",
    "__SESSION__",
    "-",
];

type SecretRefs = HashMap<String, String>;

type Aes256CbcDec = cbc::Decryptor<Aes256>;

#[derive(Clone)]
pub struct CollaborationManager {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    data_dir: PathBuf,
    active: Arc<Mutex<HashMap<String, Arc<Mutex<Child>>>>>,
    provider_tasks: Arc<Mutex<HashMap<String, tauri::async_runtime::JoinHandle<()>>>>,
    callback_task: Arc<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BotConfig {
    app_id: Option<String>,
    corp_id: Option<String>,
    agent_id: Option<String>,
    callback_public_url: Option<String>,
    application_id: Option<String>,
    guild_id: Option<String>,
}

#[derive(Debug, Clone)]
struct BotRuntime {
    stored: StoredCollaborationBot,
    config: BotConfig,
    secret_refs: SecretRefs,
}

#[derive(Debug, Deserialize)]
struct WecomCallbackQuery {
    msg_signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    echostr: Option<String>,
}

impl CollaborationManager {
    pub fn new(
        repository: Arc<Repository>,
        secrets: Arc<dyn SecretStore>,
        oauth_credentials: Arc<OAuthCredentialStore>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            repository,
            secrets,
            oauth_credentials,
            data_dir,
            active: Arc::new(Mutex::new(HashMap::new())),
            provider_tasks: Arc::new(Mutex::new(HashMap::new())),
            callback_task: Arc::new(Mutex::new(None)),
        }
    }

    pub fn spawn_enabled_connectors(&self) {
        let Ok(bots) = self.repository.list_collaboration_bots() else {
            return;
        };
        for bot in bots.into_iter().filter(|stored| stored.bot.enabled) {
            self.ensure_connector_for_bot(bot.bot.id);
        }
        self.ensure_callback_server();
    }

    pub async fn list_bots(&self) -> AppResult<Vec<MaskedCollaborationBot>> {
        Ok(self
            .repository
            .list_collaboration_bots()?
            .into_iter()
            .map(|stored| stored.bot)
            .collect())
    }

    pub async fn upsert_bot(
        &self,
        input: UpsertCollaborationBotInput,
    ) -> AppResult<MaskedCollaborationBot> {
        if !input.confirmed || input.name.trim().is_empty() {
            return Err(AppError::ValidationFailed);
        }
        let id = input
            .id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let existing = self.repository.collaboration_bot(&id).ok();
        let mut config = existing
            .as_ref()
            .map(|stored| config_from_json(&stored.config_json))
            .transpose()?
            .unwrap_or_default();
        let mut secret_refs = existing
            .as_ref()
            .map(|stored| secret_refs_from_json(&stored.secret_refs_json))
            .transpose()?
            .unwrap_or_default();

        apply_config_input(&mut config, &input);
        self.persist_provider_secrets(&id, input.provider, &input, &mut secret_refs)
            .await?;
        validate_provider_config(input.provider, &config, &secret_refs)?;

        let now = timestamp_ms();
        let callback_public_url = config.callback_public_url.clone();
        let status = if !input.enabled {
            "disabled"
        } else if input.provider == CollaborationProvider::Wecom
            && callback_public_url
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            "callback_required"
        } else {
            "configured"
        };
        let stored = StoredCollaborationBot {
            bot: MaskedCollaborationBot {
                id: id.clone(),
                provider: input.provider,
                name: input.name.trim().to_owned(),
                enabled: input.enabled,
                connection_status: status.to_owned(),
                credential_mask: credential_mask(input.provider, &config),
                config_summary: config_summary(input.provider, &config),
                callback_public_url,
                last_error: None,
                updated_at_ms: now,
            },
            config_json: serde_json::to_string(&json!({
                "app_id": config.app_id,
                "corp_id": config.corp_id,
                "agent_id": config.agent_id,
                "callback_public_url": config.callback_public_url,
                "application_id": config.application_id,
                "guild_id": config.guild_id,
            }))
            .map_err(|_| AppError::Internal)?,
            secret_refs_json: serde_json::to_string(&secret_refs)
                .map_err(|_| AppError::Internal)?,
        };
        self.repository.upsert_collaboration_bot(&stored)?;
        if input.enabled {
            self.ensure_connector_for_bot(id);
        }
        if input.provider == CollaborationProvider::Wecom {
            self.ensure_callback_server();
        }
        Ok(stored.bot)
    }

    pub async fn test_bot(&self, id: String) -> AppResult<MaskedCollaborationBot> {
        let runtime = self.runtime(&id).await?;
        match runtime.stored.bot.provider {
            CollaborationProvider::Feishu => {
                let secret = self.secret(&runtime, "app_secret").await?;
                let app_id = runtime.config.app_id.as_deref().unwrap_or_default();
                let config = Config::builder(app_id, secret)
                    .base_url(FEISHU_BASE_URL)
                    .build();
                let client =
                    feishu_sdk::Client::new(config).map_err(|_| AppError::UpstreamUnavailable)?;
                client
                    .operation("auth.v3.tenant_access_token.internal.post")
                    .body_json(&json!({"app_id": app_id, "app_secret": ""}))
                    .map_err(|_| AppError::Internal)?;
            }
            CollaborationProvider::Telegram => {
                let token = self.secret(&runtime, "bot_token").await?;
                let url = format!("https://api.telegram.org/bot{token}/getMe");
                ensure_success(Client::new().get(url).send().await?).await?;
            }
            CollaborationProvider::Discord => {
                let token = self.secret(&runtime, "bot_token").await?;
                ensure_success(
                    Client::new()
                        .get("https://discord.com/api/v10/users/@me")
                        .bearer_auth(token)
                        .send()
                        .await?,
                )
                .await?;
            }
            CollaborationProvider::Qq => {
                let _ = self.qq_access_token(&runtime).await?;
            }
            CollaborationProvider::Wecom => {
                let _ = self.wecom_access_token(&runtime).await?;
            }
        }
        self.repository
            .update_collaboration_bot_status(&id, "configured", None)?;
        Ok(self.repository.collaboration_bot(&id)?.bot)
    }

    pub async fn delete_bot(&self, input: DeleteCollaborationBotInput) -> AppResult<()> {
        if !input.confirmed {
            return Err(AppError::ConfirmationRequired);
        }
        if let Ok(mut tasks) = self.provider_tasks.lock() {
            if let Some(task) = tasks.remove(&input.id) {
                task.abort();
            }
        }
        let refs = self.repository.delete_collaboration_bot(&input.id)?;
        for reference in refs {
            self.secrets.delete(&reference).await?;
        }
        Ok(())
    }

    pub fn list_bindings(&self) -> AppResult<Vec<CollaborationProjectBinding>> {
        self.repository.list_collaboration_project_bindings()
    }

    pub fn upsert_binding(
        &self,
        input: UpsertCollaborationProjectBindingInput,
    ) -> AppResult<CollaborationProjectBinding> {
        if !input.confirmed
            || input.project_name.trim().is_empty()
            || input.project_slug.trim().is_empty()
            || input.working_directory.trim().is_empty()
        {
            return Err(AppError::ValidationFailed);
        }
        let bot = self.repository.collaboration_bot(&input.bot_id)?;
        let profile = self.repository.profile(&input.profile_id)?;
        if profile.profile.kind != ProfileKind::CodexOauth
            || !profile.profile.enabled
            || !profile.profile.credential_configured
        {
            return Err(AppError::ProfileRuntimeUnavailable);
        }
        let directory = PathBuf::from(&input.working_directory);
        if !directory.is_absolute() || !directory.is_dir() {
            return Err(AppError::ValidationFailed);
        }
        let now = timestamp_ms();
        let id = input.id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let existing = self.repository.collaboration_project_binding(&id).ok();
        let binding = CollaborationProjectBinding {
            id: id.clone(),
            provider: bot.bot.provider,
            bot_id: bot.bot.id,
            bot_name: bot.bot.name,
            project_name: input.project_name.trim().to_owned(),
            project_slug: normalize_slug(&input.project_slug)?,
            working_directory: directory.display().to_string(),
            profile_id: input.profile_id,
            profile_alias: profile.profile.alias,
            chat_id: existing.as_ref().and_then(|item| item.chat_id.clone()),
            bind_code: existing
                .as_ref()
                .map(|item| item.bind_code.clone())
                .unwrap_or_else(generate_bind_code),
            enabled: input.enabled,
            concurrency_limit: input.concurrency_limit.clamp(1, 8),
            created_at_ms: existing
                .as_ref()
                .map(|item| item.created_at_ms)
                .unwrap_or(now),
            updated_at_ms: now,
        };
        self.repository
            .upsert_collaboration_project_binding(&binding)?;
        self.repository.collaboration_project_binding(&id)
    }

    pub fn delete_binding(&self, input: DeleteCollaborationProjectBindingInput) -> AppResult<()> {
        if !input.confirmed {
            return Err(AppError::ConfirmationRequired);
        }
        self.repository
            .delete_collaboration_project_binding(&input.id)
    }

    pub fn list_sessions(
        &self,
        input: ListCodexSessionsInput,
    ) -> AppResult<Vec<CodexSessionSummary>> {
        Ok(self
            .repository
            .list_codex_sessions(input.binding_id.as_deref())?
            .into_iter()
            .map(|stored| stored.session)
            .collect())
    }

    pub async fn cancel_session(
        &self,
        input: CancelCodexSessionInput,
    ) -> AppResult<CodexSessionSummary> {
        if !input.confirmed {
            return Err(AppError::ConfirmationRequired);
        }
        self.cancel_session_by_id(&input.session_id).await
    }

    pub async fn continue_session(
        &self,
        input: ContinueCodexSessionInput,
    ) -> AppResult<CodexSessionSummary> {
        if !input.confirmed || input.instruction.trim().is_empty() {
            return Err(AppError::ValidationFailed);
        }
        let stored = self.repository.codex_session(&input.session_id)?;
        let codex_session_id = stored
            .session
            .codex_session_id
            .clone()
            .ok_or(AppError::Conflict)?;
        let binding = self
            .repository
            .collaboration_project_binding(&stored.session.binding_id)?;
        self.start_codex_session(
            binding,
            input.instruction,
            stored.session.started_by,
            true,
            Some(codex_session_id),
        )
        .await
        .map(|result| result.session.expect("continue returns session"))
    }

    pub async fn register_discord_commands(&self, id: String) -> AppResult<MaskedCollaborationBot> {
        let runtime = self.runtime(&id).await?;
        if runtime.stored.bot.provider != CollaborationProvider::Discord {
            return Err(AppError::ValidationFailed);
        }
        let token = self.secret(&runtime, "bot_token").await?;
        let application_id = runtime
            .config
            .application_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let url = if let Some(guild_id) = runtime.config.guild_id.as_deref() {
            format!("https://discord.com/api/v10/applications/{application_id}/guilds/{guild_id}/commands")
        } else {
            format!("https://discord.com/api/v10/applications/{application_id}/commands")
        };
        ensure_success(
            Client::new()
                .post(url)
                .bearer_auth(token)
                .json(&json!({
                    "name": "codex",
                    "description": "在群聊中管理 Codex Relay 任务",
                    "options": [{
                        "name": "command",
                        "description": "例如：run relay 修复测试 / projects / sessions relay",
                        "type": 3,
                        "required": true
                    }]
                }))
                .send()
                .await?,
        )
        .await?;
        self.repository
            .update_collaboration_bot_status(&id, "configured", None)?;
        Ok(self.repository.collaboration_bot(&id)?.bot)
    }

    pub fn callback_status(&self) -> AppResult<CollaborationCallbackStatus> {
        let public_urls = self
            .repository
            .list_collaboration_bots()?
            .into_iter()
            .filter(|bot| bot.bot.provider == CollaborationProvider::Wecom)
            .filter_map(|bot| bot.bot.callback_public_url)
            .collect::<Vec<_>>();
        Ok(CollaborationCallbackStatus {
            local_url: format!("http://127.0.0.1:{CALLBACK_PORT}/collaboration/wecom/<bot_id>"),
            public_urls,
            running: self
                .callback_task
                .lock()
                .ok()
                .and_then(|handle| handle.as_ref().map(|_| true))
                .unwrap_or(false),
        })
    }

    fn ensure_connector_for_bot(&self, bot_id: String) {
        if self
            .provider_tasks
            .lock()
            .ok()
            .is_some_and(|tasks| tasks.contains_key(&bot_id))
        {
            return;
        }
        let manager = self.clone();
        let bot_id_for_task = bot_id.clone();
        let handle = tauri::async_runtime::spawn(async move {
            let result = manager.run_connector(bot_id_for_task.clone()).await;
            if let Err(error) = result {
                let _ = manager.repository.update_collaboration_bot_status(
                    &bot_id_for_task,
                    "failed",
                    Some(&error.to_string()),
                );
            }
        });
        if let Ok(mut tasks) = self.provider_tasks.lock() {
            if let Some(old) = tasks.insert(bot_id, handle) {
                old.abort();
            }
        }
    }

    async fn run_connector(&self, bot_id: String) -> AppResult<()> {
        let runtime = self.runtime(&bot_id).await?;
        if !runtime.stored.bot.enabled {
            return Ok(());
        }
        match runtime.stored.bot.provider {
            CollaborationProvider::Feishu => self.run_feishu_stream(runtime).await,
            CollaborationProvider::Telegram => self.run_telegram_polling(runtime).await,
            CollaborationProvider::Discord => self.run_discord_gateway(runtime).await,
            CollaborationProvider::Qq => self.run_qq_gateway(runtime).await,
            CollaborationProvider::Wecom => {
                self.ensure_callback_server();
                self.repository.update_collaboration_bot_status(
                    &bot_id,
                    "callback_required",
                    None,
                )?;
                Ok(())
            }
        }
    }

    async fn runtime(&self, bot_id: &str) -> AppResult<BotRuntime> {
        let stored = self.repository.collaboration_bot(bot_id)?;
        Ok(BotRuntime {
            config: config_from_json(&stored.config_json)?,
            secret_refs: secret_refs_from_json(&stored.secret_refs_json)?,
            stored,
        })
    }

    async fn secret(&self, runtime: &BotRuntime, name: &str) -> AppResult<String> {
        let reference = runtime
            .secret_refs
            .get(name)
            .ok_or(AppError::ValidationFailed)?;
        self.secrets.get(reference).await
    }

    async fn persist_provider_secrets(
        &self,
        id: &str,
        provider: CollaborationProvider,
        input: &UpsertCollaborationBotInput,
        refs: &mut SecretRefs,
    ) -> AppResult<()> {
        let required = required_secret_keys(provider);
        for (key, value) in [
            ("app_secret", input.app_secret.as_ref()),
            ("client_secret", input.client_secret.as_ref()),
            ("secret", input.secret.as_ref()),
            ("token", input.token.as_ref()),
            ("encoding_aes_key", input.encoding_aes_key.as_ref()),
            ("bot_token", input.bot_token.as_ref()),
        ] {
            let Some(value) = value
                .map(String::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let reference = refs
                .get(key)
                .cloned()
                .unwrap_or_else(|| format!("collaboration:{id}:{key}"));
            self.secrets.set(&reference, value).await?;
            refs.insert(key.to_owned(), reference);
        }
        for key in required {
            if !refs.contains_key(*key) {
                return Err(AppError::ValidationFailed);
            }
        }
        Ok(())
    }

    async fn handle_incoming_text(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        sender: Option<&str>,
        text: &str,
    ) -> AppResult<()> {
        if !text.trim_start().starts_with("/codex") {
            return Ok(());
        }
        let result = self
            .handle_command(provider, bot_id, chat_id, sender, text)
            .await
            .unwrap_or_else(|error| CollaborationCommandResult {
                status: "failed".to_owned(),
                message: error.to_string(),
                session: None,
            });
        if let Some(session) = result.session.as_ref() {
            let _ = self.send_or_update_session_message(session).await;
        } else {
            let _ = self
                .send_platform_text(provider, bot_id, chat_id, &result.message)
                .await;
        }
        Ok(())
    }

    pub async fn handle_command(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        sender: Option<&str>,
        text: &str,
    ) -> AppResult<CollaborationCommandResult> {
        match parse_codex_command(text)? {
            CodexCommand::Help => Ok(CollaborationCommandResult {
                status: "ok".into(),
                message: help_text(provider),
                session: None,
            }),
            CodexCommand::Bind { code } => {
                let binding = self.repository.collaboration_binding_by_code(&code)?;
                if binding.provider != provider || binding.bot_id != bot_id {
                    return Err(AppError::NotFound);
                }
                let binding = self
                    .repository
                    .bind_collaboration_project_chat(&binding.id, chat_id)?;
                Ok(CollaborationCommandResult {
                    status: "ok".into(),
                    message: format!("项目“{}”已绑定到当前会话。", binding.project_name),
                    session: None,
                })
            }
            CodexCommand::Projects => {
                let bindings = self
                    .repository
                    .collaboration_bindings_for_chat(provider, bot_id, chat_id)?;
                let message = if bindings.is_empty() {
                    "当前会话还没有绑定项目。请先在 Codex Relay 客户端创建项目绑定，再发送 /codex bind <code>。".to_owned()
                } else {
                    format!(
                        "可用项目：\n{}",
                        bindings
                            .into_iter()
                            .map(|binding| format!(
                                "- {} ({})",
                                binding.project_name, binding.project_slug
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                Ok(CollaborationCommandResult {
                    status: "ok".into(),
                    message,
                    session: None,
                })
            }
            CodexCommand::Run {
                project,
                instruction,
            } => {
                let binding = self.binding_for_project(provider, bot_id, chat_id, &project)?;
                self.start_codex_session(
                    binding,
                    instruction,
                    sender.map(ToOwned::to_owned),
                    false,
                    None,
                )
                .await
            }
            CodexCommand::Sessions { project } => {
                let binding_id = match project {
                    Some(project) => Some(
                        self.binding_for_project(provider, bot_id, chat_id, &project)?
                            .id,
                    ),
                    None => None,
                };
                let sessions = self.repository.list_codex_sessions(binding_id.as_deref())?;
                let chat_sessions = sessions
                    .into_iter()
                    .filter(|stored| {
                        stored.session.provider == provider
                            && stored.session.provider_bot_id.as_deref() == Some(bot_id)
                            && stored.session.provider_chat_id.as_deref() == Some(chat_id)
                    })
                    .take(10)
                    .map(|stored| {
                        format!(
                            "- {} [{}] {}",
                            short_id(&stored.session.id),
                            stored.session.relay_status,
                            stored.session.summary.unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>();
                Ok(CollaborationCommandResult {
                    status: "ok".into(),
                    message: if chat_sessions.is_empty() {
                        "暂无会话。".into()
                    } else {
                        chat_sessions.join("\n")
                    },
                    session: None,
                })
            }
            CodexCommand::Status { session_id } => {
                let session = self.repository.codex_session(&session_id)?.session;
                Ok(CollaborationCommandResult {
                    status: "ok".into(),
                    message: format_session_status(&session),
                    session: None,
                })
            }
            CodexCommand::Cancel { session_id } => self
                .cancel_session_by_id(&session_id)
                .await
                .map(|session| CollaborationCommandResult {
                    status: "cancelled".into(),
                    message: "任务已取消。".into(),
                    session: Some(session),
                }),
            CodexCommand::Continue {
                session_id,
                instruction,
            } => {
                let stored = self.repository.codex_session(&session_id)?;
                let codex_session_id = stored
                    .session
                    .codex_session_id
                    .clone()
                    .ok_or(AppError::Conflict)?;
                let binding = self
                    .repository
                    .collaboration_project_binding(&stored.session.binding_id)?;
                self.start_codex_session(
                    binding,
                    instruction,
                    sender.map(ToOwned::to_owned),
                    true,
                    Some(codex_session_id),
                )
                .await
            }
        }
    }

    fn binding_for_project(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        project: &str,
    ) -> AppResult<CollaborationProjectBinding> {
        self.repository
            .collaboration_bindings_for_chat(provider, bot_id, chat_id)?
            .into_iter()
            .find(|binding| binding.project_slug == project && binding.enabled)
            .ok_or(AppError::NotFound)
    }

    async fn cancel_session_by_id(&self, session_id: &str) -> AppResult<CodexSessionSummary> {
        let child = self
            .active
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(session_id);
        if let Some(child) = child {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
            }
        }
        let session = self.repository.set_codex_session_status(
            session_id,
            "cancelled",
            Some("任务已取消。"),
            None,
            Some(timestamp_ms()),
        )?;
        self.record_event(session_id, "cancelled", "任务已取消。")?;
        let manager = self.clone();
        let session_for_update = session.clone();
        tauri::async_runtime::spawn(async move {
            let _ = manager.update_session_message(&session_for_update).await;
        });
        Ok(session)
    }

    async fn start_codex_session(
        &self,
        binding: CollaborationProjectBinding,
        instruction: String,
        started_by: Option<String>,
        resume: bool,
        codex_session_id: Option<String>,
    ) -> AppResult<CollaborationCommandResult> {
        if self.repository.count_running_codex_sessions(None)? >= GLOBAL_CONCURRENCY_LIMIT {
            return Ok(CollaborationCommandResult {
                status: "limited".into(),
                message: format!("全局并发已达上限 {GLOBAL_CONCURRENCY_LIMIT}，请稍后重试。"),
                session: None,
            });
        }
        let project_limit = binding
            .concurrency_limit
            .max(1)
            .min(DEFAULT_PROJECT_CONCURRENCY_LIMIT.max(binding.concurrency_limit));
        if self
            .repository
            .count_running_codex_sessions(Some(&binding.id))?
            >= project_limit
        {
            return Ok(CollaborationCommandResult {
                status: "limited".into(),
                message: format!(
                    "项目“{}”并发已达上限 {project_limit}，请稍后重试。",
                    binding.project_name
                ),
                session: None,
            });
        }
        let profile = self.repository.profile(&binding.profile_id)?;
        let auth_json = self.profile_auth_json(&profile).await?;
        let session_id = Uuid::new_v4().to_string();
        let now = timestamp_ms();
        let session_home = self.session_home(&session_id);
        write_auth_json_to_home(&session_home, &auth_json)?;
        let output_file = session_home.join("last-message.txt");
        let mut command = Command::new("codex");
        if resume {
            let codex_id = codex_session_id.clone().ok_or(AppError::ValidationFailed)?;
            let args = CODEX_RESUME_ARGS
                .iter()
                .flat_map(|arg| match *arg {
                    "__OUTPUT__" => vec![output_file.display().to_string()],
                    "__SESSION__" => vec![codex_id.clone()],
                    other => vec![other.to_owned()],
                })
                .collect::<Vec<_>>();
            command.args(args);
        } else {
            let args = CODEX_SESSION_ARGS
                .iter()
                .flat_map(|arg| match *arg {
                    "__OUTPUT__" => vec![output_file.display().to_string(), "-".to_owned()],
                    other => vec![other.to_owned()],
                })
                .collect::<Vec<_>>();
            command.args(args);
        }
        command
            .env("CODEX_HOME", &session_home)
            .env_remove("CODEX_API_KEY")
            .current_dir(&binding.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| AppError::RuntimeUnavailable)?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(instruction.as_bytes())
                .and_then(|()| stdin.flush())
                .map_err(|_| AppError::RuntimeUnavailable)?;
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let session = CodexSessionSummary {
            id: session_id.clone(),
            binding_id: binding.id.clone(),
            provider: binding.provider,
            provider_bot_id: Some(binding.bot_id.clone()),
            provider_chat_id: binding.chat_id.clone(),
            provider_message_id: None,
            project_name: binding.project_name.clone(),
            project_slug: binding.project_slug.clone(),
            profile_id: binding.profile_id.clone(),
            profile_alias: binding.profile_alias.clone(),
            relay_status: "running".into(),
            codex_session_id,
            feishu_message_id: None,
            feishu_chat_id: if binding.provider == CollaborationProvider::Feishu {
                binding.chat_id.clone()
            } else {
                None
            },
            started_by,
            started_at_ms: now,
            updated_at_ms: now,
            finished_at_ms: None,
            summary: Some(
                if resume {
                    "继续会话中。"
                } else {
                    "任务已启动。"
                }
                .into(),
            ),
            last_error: None,
        };
        self.repository.insert_codex_session(&StoredCodexSession {
            session: session.clone(),
            working_directory: binding.working_directory.clone(),
        })?;
        self.record_event(&session_id, "received", &redact(&instruction))?;
        let child = Arc::new(Mutex::new(child));
        self.active
            .lock()
            .map_err(|_| AppError::Internal)?
            .insert(session_id.clone(), child.clone());
        self.spawn_session_watcher(session_id.clone(), child, stdout, stderr, output_file);
        Ok(CollaborationCommandResult {
            status: "running".into(),
            message: "Codex 任务已启动。".into(),
            session: Some(session),
        })
    }

    fn spawn_session_watcher(
        &self,
        session_id: String,
        child: Arc<Mutex<Child>>,
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
        output_file: PathBuf,
    ) {
        let repository = self.repository.clone();
        let active = self.active.clone();
        let manager = self.clone();
        thread::spawn(move || {
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let content = redact(&line);
                    let _ = repository.insert_codex_session_event(&CodexSessionEvent {
                        id: Uuid::new_v4().to_string(),
                        session_id: session_id.clone(),
                        occurred_at_ms: timestamp_ms(),
                        event_type: "stdout".into(),
                        content: content.clone(),
                    });
                    if let Some(codex_id) = parse_codex_session_id(&line) {
                        let _ = repository.set_codex_session_codex_id(&session_id, &codex_id);
                    }
                }
            }
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        let _ = repository.insert_codex_session_event(&CodexSessionEvent {
                            id: Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            occurred_at_ms: timestamp_ms(),
                            event_type: "stderr".into(),
                            content: redact(&line),
                        });
                    }
                }
            }
            let success = child
                .lock()
                .ok()
                .and_then(|mut child| child.wait().ok())
                .map(|status| status.success())
                .unwrap_or(false);
            if let Ok(mut active) = active.lock() {
                active.remove(&session_id);
            }
            let final_summary = fs::read_to_string(&output_file)
                .ok()
                .map(|value| redact(&value))
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    if success {
                        "任务已完成。"
                    } else {
                        "任务未成功完成。"
                    }
                    .into()
                });
            let status = if success { "completed" } else { "failed" };
            let session = repository.set_codex_session_status(
                &session_id,
                status,
                Some(&truncate_chars(&final_summary, 1500)),
                if success {
                    None
                } else {
                    Some("Codex 进程返回失败状态。")
                },
                Some(timestamp_ms()),
            );
            let _ = repository.insert_codex_session_event(&CodexSessionEvent {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                occurred_at_ms: timestamp_ms(),
                event_type: status.into(),
                content: final_summary,
            });
            if let Ok(session) = session {
                tauri::async_runtime::spawn(async move {
                    let _ = manager.update_session_message(&session).await;
                });
            }
        });
    }

    async fn profile_auth_json(
        &self,
        profile: &crate::database::StoredProfile,
    ) -> AppResult<String> {
        if profile.profile.auth_mode == CodexAuthMode::OAuth {
            let credential = self
                .oauth_credentials
                .load(profile, OAuthCredentialAccess::Background)
                .await?;
            return credential.auth_json();
        }
        let reference = profile
            .secret_ref
            .as_deref()
            .ok_or(AppError::ProfileRuntimeUnavailable)?;
        let encoded = self.secrets.get(reference).await?;
        serde_json::from_str::<ImportedAuthFileCredential>(&encoded)
            .map(|credential| credential.auth_json)
            .or_else(|_| {
                serde_json::from_str::<CodexOAuthCredential>(&encoded).and_then(|credential| {
                    credential
                        .auth_json()
                        .map_err(|_| serde_json::Error::io(std::io::Error::other("auth_json")))
                })
            })
            .map_err(|_| AppError::ProfileRuntimeUnavailable)
    }

    fn session_home(&self, session_id: &str) -> PathBuf {
        self.data_dir
            .join("collaboration-sessions")
            .join(session_id)
            .join("codex-home")
    }

    fn record_event(&self, session_id: &str, event_type: &str, content: &str) -> AppResult<()> {
        self.repository
            .insert_codex_session_event(&CodexSessionEvent {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.to_owned(),
                occurred_at_ms: timestamp_ms(),
                event_type: event_type.to_owned(),
                content: content.to_owned(),
            })
    }

    async fn send_or_update_session_message(&self, session: &CodexSessionSummary) -> AppResult<()> {
        if session.provider_message_id.is_some() || session.feishu_message_id.is_some() {
            self.update_session_message(session).await
        } else {
            self.send_session_message(session).await.map(|_| ())
        }
    }

    async fn send_session_message(&self, session: &CodexSessionSummary) -> AppResult<String> {
        let bot_id = session
            .provider_bot_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let chat_id = session
            .provider_chat_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let message_id = if session.provider == CollaborationProvider::Feishu {
            self.send_feishu_card(bot_id, chat_id, session).await?
        } else {
            self.send_platform_text(
                session.provider,
                bot_id,
                chat_id,
                &format_session_status(session),
            )
            .await?
        };
        if !message_id.is_empty() {
            self.repository
                .set_codex_session_provider_message(&session.id, &message_id)?;
        }
        Ok(message_id)
    }

    async fn update_session_message(&self, session: &CodexSessionSummary) -> AppResult<()> {
        match session.provider {
            CollaborationProvider::Feishu => {
                let message_id = session
                    .provider_message_id
                    .as_deref()
                    .or(session.feishu_message_id.as_deref())
                    .ok_or(AppError::ValidationFailed)?;
                let bot_id = session
                    .provider_bot_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                self.update_feishu_card(bot_id, message_id, session).await
            }
            CollaborationProvider::Telegram => {
                let bot_id = session
                    .provider_bot_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                let chat_id = session
                    .provider_chat_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                let message_id = session
                    .provider_message_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                self.edit_telegram_message(
                    bot_id,
                    chat_id,
                    message_id,
                    &format_session_status(session),
                )
                .await
            }
            CollaborationProvider::Discord => {
                let bot_id = session
                    .provider_bot_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                let chat_id = session
                    .provider_chat_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                let message_id = session
                    .provider_message_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                self.edit_discord_message(
                    bot_id,
                    chat_id,
                    message_id,
                    &format_session_status(session),
                )
                .await
            }
            _ => {
                let bot_id = session
                    .provider_bot_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                let chat_id = session
                    .provider_chat_id
                    .as_deref()
                    .ok_or(AppError::ValidationFailed)?;
                self.send_platform_text(
                    session.provider,
                    bot_id,
                    chat_id,
                    &format_session_status(session),
                )
                .await
                .map(|_| ())
            }
        }
    }

    async fn send_platform_text(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        text: &str,
    ) -> AppResult<String> {
        match provider {
            CollaborationProvider::Feishu => self.send_feishu_text(bot_id, chat_id, text).await,
            CollaborationProvider::Telegram => {
                self.send_telegram_message(bot_id, chat_id, text).await
            }
            CollaborationProvider::Discord => {
                self.send_discord_message(bot_id, chat_id, text).await
            }
            CollaborationProvider::Qq => self.send_qq_message(bot_id, chat_id, text).await,
            CollaborationProvider::Wecom => self.send_wecom_message(bot_id, chat_id, text).await,
        }
    }

    async fn run_feishu_stream(&self, runtime: BotRuntime) -> AppResult<()> {
        let app_id = runtime
            .config
            .app_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let secret = self.secret(&runtime, "app_secret").await?;
        let config = Config::builder(app_id, secret)
            .base_url(FEISHU_BASE_URL)
            .build();
        let client = feishu_sdk::Client::new(config).map_err(|_| AppError::UpstreamUnavailable)?;
        let dispatcher = EventDispatcher::new(EventDispatcherConfig::new(), noop_logger());
        dispatcher
            .register_handler(Box::new(FeishuMessageHandler {
                bot_id: runtime.stored.bot.id.clone(),
                manager: self.clone(),
            }))
            .await;
        let card_handler = CardActionHandler::new(noop_logger()).handler({
            let manager = self.clone();
            move |action| {
                let manager = manager.clone();
                async move {
                    manager
                        .handle_feishu_card_action(action)
                        .await
                        .map(|()| None)
                        .map_err(|error| {
                            feishu_sdk::core::Error::InvalidCardActionFormat(error.to_string())
                        })
                }
            }
        });
        self.repository.update_collaboration_bot_status(
            &runtime.stored.bot.id,
            "connecting",
            None,
        )?;
        let stream = client
            .stream()
            .event_dispatcher(dispatcher)
            .card_handler(card_handler)
            .build()
            .map_err(|_| AppError::UpstreamUnavailable)?;
        self.repository.update_collaboration_bot_status(
            &runtime.stored.bot.id,
            "connected",
            None,
        )?;
        stream
            .start()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)
    }

    async fn handle_feishu_card_action(&self, action: CardAction) -> AppResult<()> {
        let Some(value) = action.action.and_then(|item| item.value) else {
            return Ok(());
        };
        let op = value.get("op").and_then(Value::as_str).unwrap_or_default();
        let session_id = value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match op {
            "cancel" if !session_id.is_empty() => {
                let _ = self.cancel_session_by_id(session_id).await?;
            }
            "refresh" if !session_id.is_empty() => {
                let session = self.repository.codex_session(session_id)?.session;
                self.update_session_message(&session).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn send_feishu_card(
        &self,
        bot_id: &str,
        chat_id: &str,
        session: &CodexSessionSummary,
    ) -> AppResult<String> {
        let response = self
            .feishu_client(bot_id)
            .await?
            .operation("im.v1.message.create")
            .query_param("receive_id_type", "chat_id")
            .body_json(&json!({
                "receive_id": chat_id,
                "msg_type": "interactive",
                "content": session_card(session).to_string(),
            }))
            .map_err(|_| AppError::Internal)?
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        let json = response
            .json_value()
            .map_err(|_| AppError::UpstreamUnavailable)?;
        Ok(json
            .get("data")
            .and_then(|data| data.get("message_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn update_feishu_card(
        &self,
        bot_id: &str,
        message_id: &str,
        session: &CodexSessionSummary,
    ) -> AppResult<()> {
        self.feishu_client(bot_id)
            .await?
            .operation("im.v1.message.patch")
            .path_param("message_id", message_id)
            .body_json(&json!({"content": session_card(session).to_string()}))
            .map_err(|_| AppError::Internal)?
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        Ok(())
    }

    async fn send_feishu_text(&self, bot_id: &str, chat_id: &str, text: &str) -> AppResult<String> {
        let response = self
            .feishu_client(bot_id)
            .await?
            .operation("im.v1.message.create")
            .query_param("receive_id_type", "chat_id")
            .body_json(&json!({
                "receive_id": chat_id,
                "msg_type": "text",
                "content": json!({"text": truncate_chars(text, 3000)}).to_string(),
            }))
            .map_err(|_| AppError::Internal)?
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        let json = response
            .json_value()
            .map_err(|_| AppError::UpstreamUnavailable)?;
        Ok(json
            .get("data")
            .and_then(|data| data.get("message_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn feishu_client(&self, bot_id: &str) -> AppResult<feishu_sdk::Client> {
        let runtime = self.runtime(bot_id).await?;
        let app_id = runtime
            .config
            .app_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let secret = self.secret(&runtime, "app_secret").await?;
        let config = Config::builder(app_id, secret)
            .base_url(FEISHU_BASE_URL)
            .build();
        feishu_sdk::Client::new(config).map_err(|_| AppError::UpstreamUnavailable)
    }

    async fn run_telegram_polling(&self, runtime: BotRuntime) -> AppResult<()> {
        let bot_id = runtime.stored.bot.id.clone();
        let token = self.secret(&runtime, "bot_token").await?;
        self.repository
            .update_collaboration_bot_status(&bot_id, "connected", None)?;
        let client = Client::new();
        loop {
            let offset = self
                .repository
                .provider_state(CollaborationProvider::Telegram, &bot_id, "offset")?
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0);
            let response = client
                .get(format!("https://api.telegram.org/bot{token}/getUpdates"))
                .query(&[("timeout", "25"), ("offset", &offset.to_string())])
                .send()
                .await
                .map_err(|_| AppError::UpstreamUnavailable)?;
            let body: Value = response
                .json()
                .await
                .map_err(|_| AppError::UpstreamUnavailable)?;
            if let Some(updates) = body.get("result").and_then(Value::as_array) {
                for update in updates {
                    if let Some(update_id) = update.get("update_id").and_then(Value::as_i64) {
                        self.repository.set_provider_state(
                            CollaborationProvider::Telegram,
                            &bot_id,
                            "offset",
                            &(update_id + 1).to_string(),
                        )?;
                    }
                    let Some(message) =
                        update.get("message").or_else(|| update.get("channel_post"))
                    else {
                        continue;
                    };
                    let text = message
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let chat_id = message
                        .get("chat")
                        .and_then(|chat| chat.get("id"))
                        .map(value_to_id_string)
                        .unwrap_or_default();
                    let sender = message
                        .get("from")
                        .and_then(|from| from.get("username").or_else(|| from.get("id")))
                        .map(value_to_id_string);
                    if !chat_id.is_empty() {
                        let _ = self
                            .handle_incoming_text(
                                CollaborationProvider::Telegram,
                                &bot_id,
                                &chat_id,
                                sender.as_deref(),
                                text,
                            )
                            .await;
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn send_telegram_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
    ) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        let response: Value = Client::new()
            .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
            .json(&json!({"chat_id": chat_id, "text": truncate_chars(text, 3900)}))
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        Ok(response
            .get("result")
            .and_then(|result| result.get("message_id"))
            .map(value_to_id_string)
            .unwrap_or_default())
    }

    async fn edit_telegram_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> AppResult<()> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        ensure_success(
            Client::new()
                .post(format!("https://api.telegram.org/bot{token}/editMessageText"))
                .json(&json!({"chat_id": chat_id, "message_id": message_id, "text": truncate_chars(text, 3900)}))
                .send()
                .await?,
        )
        .await
    }

    async fn run_discord_gateway(&self, runtime: BotRuntime) -> AppResult<()> {
        let bot_id = runtime.stored.bot.id.clone();
        let token = self.secret(&runtime, "bot_token").await?;
        let gateway: Value = Client::new()
            .get("https://discord.com/api/v10/gateway/bot")
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        let url = gateway
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("wss://gateway.discord.gg")
            .to_owned()
            + "/?v=10&encoding=json";
        self.repository
            .update_collaboration_bot_status(&bot_id, "connecting", None)?;
        let (ws, _) = connect_async(url)
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        let (mut write, mut read) = ws.split();
        let identify = json!({
            "op": 2,
            "d": {
                "token": token,
                "intents": 33280,
                "properties": {"os": "codex-relay", "browser": "codex-relay", "device": "codex-relay"}
            }
        });
        write
            .send(Message::Text(identify.to_string().into()))
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        self.repository
            .update_collaboration_bot_status(&bot_id, "connected", None)?;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
        heartbeat.tick().await;
        let mut heartbeat_started = false;
        loop {
            tokio::select! {
                _ = heartbeat.tick(), if heartbeat_started => {
                    write
                        .send(Message::Text(json!({"op": 1, "d": null}).to_string().into()))
                        .await
                        .map_err(|_| AppError::UpstreamUnavailable)?;
                }
                message = read.next() => {
                    let Some(Ok(Message::Text(text))) = message else {
                        continue;
                    };
                    let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if payload.get("op").and_then(Value::as_i64) == Some(10) {
                        let interval = payload
                            .get("d")
                            .and_then(|d| d.get("heartbeat_interval"))
                            .and_then(Value::as_u64)
                            .unwrap_or(45_000);
                        heartbeat = tokio::time::interval(Duration::from_millis(interval));
                        heartbeat.tick().await;
                        heartbeat_started = true;
                        continue;
                    }
                    let event_type = payload.get("t").and_then(Value::as_str).unwrap_or_default();
                    match event_type {
                "MESSAGE_CREATE" => {
                    let d = payload.get("d").unwrap_or(&Value::Null);
                    if d.get("author").and_then(|a| a.get("bot")).and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    let text = d.get("content").and_then(Value::as_str).unwrap_or_default();
                    let chat_id = d.get("channel_id").and_then(Value::as_str).unwrap_or_default();
                    let sender = d
                        .get("author")
                        .and_then(|a| a.get("username").or_else(|| a.get("id")))
                        .map(value_to_id_string);
                    let _ = self
                        .handle_incoming_text(
                            CollaborationProvider::Discord,
                            &bot_id,
                            chat_id,
                            sender.as_deref(),
                            text,
                        )
                        .await;
                }
                "INTERACTION_CREATE" => {
                    let d = payload.get("d").unwrap_or(&Value::Null);
                    let name = d.get("data").and_then(|data| data.get("name")).and_then(Value::as_str).unwrap_or_default();
                    if name != "codex" {
                        continue;
                    }
                    let option = d
                        .get("data")
                        .and_then(|data| data.get("options"))
                        .and_then(Value::as_array)
                        .and_then(|options| options.first())
                        .and_then(|option| option.get("value"))
                        .and_then(Value::as_str)
                        .unwrap_or("help");
                    let text = format!("/codex {option}");
                    let chat_id = d.get("channel_id").and_then(Value::as_str).unwrap_or_default();
                    let sender = d
                        .get("member")
                        .and_then(|m| m.get("user"))
                        .and_then(|u| u.get("username").or_else(|| u.get("id")))
                        .map(value_to_id_string);
                    self.ack_discord_interaction(d).await.ok();
                    let _ = self
                        .handle_incoming_text(
                            CollaborationProvider::Discord,
                            &bot_id,
                            chat_id,
                            sender.as_deref(),
                            &text,
                        )
                        .await;
                }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn ack_discord_interaction(&self, interaction: &Value) -> AppResult<()> {
        let id = interaction
            .get("id")
            .and_then(Value::as_str)
            .ok_or(AppError::ValidationFailed)?;
        let token = interaction
            .get("token")
            .and_then(Value::as_str)
            .ok_or(AppError::ValidationFailed)?;
        ensure_success(
            Client::new()
                .post(format!("https://discord.com/api/v10/interactions/{id}/{token}/callback"))
                .json(&json!({"type": 4, "data": {"content": "Codex Relay 已接收命令，结果会发送到当前频道。"}}))
                .send()
                .await?,
        )
        .await
    }

    async fn send_discord_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
    ) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        let response: Value = Client::new()
            .post(format!(
                "https://discord.com/api/v10/channels/{chat_id}/messages"
            ))
            .bearer_auth(token)
            .json(&json!({"content": truncate_chars(text, 1900)}))
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        Ok(response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn edit_discord_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> AppResult<()> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        ensure_success(
            Client::new()
                .patch(format!(
                    "https://discord.com/api/v10/channels/{chat_id}/messages/{message_id}"
                ))
                .bearer_auth(token)
                .json(&json!({"content": truncate_chars(text, 1900)}))
                .send()
                .await?,
        )
        .await
    }

    async fn run_qq_gateway(&self, runtime: BotRuntime) -> AppResult<()> {
        let bot_id = runtime.stored.bot.id.clone();
        let token = self.qq_access_token(&runtime).await?;
        let gateway: Value = Client::new()
            .get("https://api.sgroup.qq.com/gateway")
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        let url = gateway
            .get("url")
            .and_then(Value::as_str)
            .ok_or(AppError::UpstreamUnavailable)?;
        self.repository
            .update_collaboration_bot_status(&bot_id, "connecting", None)?;
        let (ws, _) = connect_async(url)
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        let (mut write, mut read) = ws.split();
        write
            .send(Message::Text(json!({"op": 2, "d": {"token": format!("QQBot {token}"), "intents": 1 << 25, "shard": [0, 1], "properties": {}}}).to_string().into()))
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        self.repository
            .update_collaboration_bot_status(&bot_id, "connected", None)?;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
        heartbeat.tick().await;
        let mut heartbeat_started = false;
        loop {
            tokio::select! {
                _ = heartbeat.tick(), if heartbeat_started => {
                    write
                        .send(Message::Text(json!({"op": 1, "d": null}).to_string().into()))
                        .await
                        .map_err(|_| AppError::UpstreamUnavailable)?;
                }
                message = read.next() => {
                    let Some(Ok(Message::Text(text))) = message else { continue; };
                    let Ok(payload) = serde_json::from_str::<Value>(&text) else { continue; };
                    if payload.get("op").and_then(Value::as_i64) == Some(10) {
                        let interval = payload
                            .get("d")
                            .and_then(|d| d.get("heartbeat_interval"))
                            .and_then(Value::as_u64)
                            .unwrap_or(45_000);
                        heartbeat = tokio::time::interval(Duration::from_millis(interval));
                        heartbeat.tick().await;
                        heartbeat_started = true;
                        continue;
                    }
                    let event_type = payload.get("t").and_then(Value::as_str).unwrap_or_default();
                    if matches!(event_type, "GROUP_AT_MESSAGE_CREATE" | "AT_MESSAGE_CREATE" | "MESSAGE_CREATE") {
                        let d = payload.get("d").unwrap_or(&Value::Null);
                        let text = d.get("content").and_then(Value::as_str).unwrap_or_default().trim();
                        let chat_id = d
                            .get("group_openid")
                            .or_else(|| d.get("channel_id"))
                            .or_else(|| d.get("guild_id"))
                            .map(value_to_id_string)
                            .unwrap_or_default();
                        let sender = d
                            .get("author")
                            .and_then(|a| a.get("member_openid").or_else(|| a.get("id")))
                            .map(value_to_id_string);
                        let _ = self
                            .handle_incoming_text(
                                CollaborationProvider::Qq,
                                &bot_id,
                                &chat_id,
                                sender.as_deref(),
                                text,
                            )
                            .await;
                    }
                }
            }
        }
    }

    async fn qq_access_token(&self, runtime: &BotRuntime) -> AppResult<String> {
        let app_id = runtime
            .config
            .app_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let client_secret = self.secret(runtime, "client_secret").await?;
        let response: Value = Client::new()
            .post("https://bots.qq.com/app/getAppAccessToken")
            .json(&json!({"appId": app_id, "clientSecret": client_secret}))
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        response
            .get("access_token")
            .or_else(|| response.get("accessToken"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or(AppError::UpstreamUnavailable)
    }

    async fn send_qq_message(&self, bot_id: &str, chat_id: &str, text: &str) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.qq_access_token(&runtime).await?;
        let response: Value = Client::new()
            .post(format!(
                "https://api.sgroup.qq.com/v2/groups/{chat_id}/messages"
            ))
            .bearer_auth(token)
            .json(&json!({"content": truncate_chars(text, 1900), "msg_type": 0}))
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        Ok(response
            .get("id")
            .or_else(|| response.get("message_id"))
            .map(value_to_id_string)
            .unwrap_or_default())
    }

    fn ensure_callback_server(&self) {
        if self
            .callback_task
            .lock()
            .ok()
            .and_then(|task| task.as_ref().map(|_| true))
            .unwrap_or(false)
        {
            return;
        }
        let manager = self.clone();
        let handle = tauri::async_runtime::spawn(async move {
            let app = Router::new()
                .route(
                    "/collaboration/wecom/:bot_id",
                    get(wecom_verify).post(wecom_receive),
                )
                .with_state(manager);
            let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await
            else {
                return;
            };
            let _ = axum::serve(listener, app).await;
        });
        if let Ok(mut task) = self.callback_task.lock() {
            *task = Some(handle);
        }
    }

    async fn wecom_access_token(&self, runtime: &BotRuntime) -> AppResult<String> {
        let corp_id = runtime
            .config
            .corp_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let secret = self.secret(runtime, "secret").await?;
        let response: Value = Client::new()
            .get("https://qyapi.weixin.qq.com/cgi-bin/gettoken")
            .query(&[("corpid", corp_id), ("corpsecret", secret.as_str())])
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?
            .json()
            .await
            .map_err(|_| AppError::UpstreamUnavailable)?;
        response
            .get("access_token")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or(AppError::UpstreamUnavailable)
    }

    async fn send_wecom_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
    ) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.wecom_access_token(&runtime).await?;
        let agent_id = runtime
            .config
            .agent_id
            .as_deref()
            .ok_or(AppError::ValidationFailed)?;
        let mut body = json!({
            "msgtype": "text",
            "agentid": agent_id.parse::<i64>().unwrap_or_default(),
            "text": {"content": truncate_chars(text, 1900)},
            "safe": 0
        });
        if chat_id.starts_with("wr") || chat_id.starts_with("CHAT") || chat_id.contains('@') {
            body["touser"] = json!(chat_id);
        } else {
            body["chatid"] = json!(chat_id);
        }
        ensure_success(
            Client::new()
                .post(format!(
                    "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token={token}"
                ))
                .json(&body)
                .send()
                .await?,
        )
        .await?;
        Ok(String::new())
    }

    async fn handle_wecom_verify(
        &self,
        bot_id: &str,
        query: WecomCallbackQuery,
    ) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "token").await?;
        let aes_key = self.secret(&runtime, "encoding_aes_key").await?;
        let echostr = query.echostr.ok_or(AppError::ValidationFailed)?;
        verify_wecom_signature(
            &token,
            query.timestamp.as_deref().unwrap_or_default(),
            query.nonce.as_deref().unwrap_or_default(),
            &echostr,
            query.msg_signature.as_deref().unwrap_or_default(),
        )?;
        decrypt_wecom(&aes_key, &echostr).map(|(message, _)| message)
    }

    async fn handle_wecom_receive(
        &self,
        bot_id: &str,
        query: WecomCallbackQuery,
        body: String,
    ) -> AppResult<()> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "token").await?;
        let aes_key = self.secret(&runtime, "encoding_aes_key").await?;
        let encrypt = xml_tag(&body, "Encrypt").ok_or(AppError::ValidationFailed)?;
        verify_wecom_signature(
            &token,
            query.timestamp.as_deref().unwrap_or_default(),
            query.nonce.as_deref().unwrap_or_default(),
            &encrypt,
            query.msg_signature.as_deref().unwrap_or_default(),
        )?;
        let (xml, _) = decrypt_wecom(&aes_key, &encrypt)?;
        let content = xml_tag(&xml, "Content").unwrap_or_default();
        let from = xml_tag(&xml, "FromUserName").unwrap_or_else(|| "unknown".to_owned());
        let chat_id = xml_tag(&xml, "ChatId")
            .or_else(|| xml_tag(&xml, "FromUserName"))
            .unwrap_or_default();
        if !chat_id.is_empty() {
            self.handle_incoming_text(
                CollaborationProvider::Wecom,
                bot_id,
                &chat_id,
                Some(&from),
                &content,
            )
            .await?;
        }
        Ok(())
    }
}

async fn wecom_verify(
    AxumPath(bot_id): AxumPath<String>,
    Query(query): Query<WecomCallbackQuery>,
    AxumState(manager): AxumState<CollaborationManager>,
) -> impl IntoResponse {
    match manager.handle_wecom_verify(&bot_id, query).await {
        Ok(echo) => (StatusCode::OK, echo).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

async fn wecom_receive(
    AxumPath(bot_id): AxumPath<String>,
    Query(query): Query<WecomCallbackQuery>,
    AxumState(manager): AxumState<CollaborationManager>,
    body: String,
) -> impl IntoResponse {
    match manager.handle_wecom_receive(&bot_id, query, body).await {
        Ok(()) => (StatusCode::OK, "success").into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

struct FeishuMessageHandler {
    bot_id: String,
    manager: CollaborationManager,
}

impl EventHandler for FeishuMessageHandler {
    fn event_type(&self) -> &str {
        "im.message.receive_v1"
    }

    fn handle(
        &self,
        event: Event,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EventHandlerResult> + Send + '_>> {
        Box::pin(async move {
            self.manager
                .handle_feishu_group_message(&self.bot_id, event)
                .await
                .map_err(|error| feishu_sdk::core::Error::InvalidEventFormat(error.to_string()))?;
            Ok(Some(EventResp::ok(b"{\"success\":true}".to_vec())))
        })
    }
}

impl CollaborationManager {
    async fn handle_feishu_group_message(&self, bot_id: &str, event: Event) -> AppResult<()> {
        let value = serde_json::to_value(event.event).map_err(|_| AppError::Internal)?;
        let Some(message) = value.get("message") else {
            return Ok(());
        };
        let chat_id = message
            .get("chat_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if chat_id.is_empty() {
            return Ok(());
        }
        let sender = value
            .get("sender")
            .and_then(|sender| sender.get("sender_id"))
            .and_then(|sender_id| sender_id.get("open_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let text = extract_feishu_message_text(message).unwrap_or_default();
        self.handle_incoming_text(
            CollaborationProvider::Feishu,
            bot_id,
            &chat_id,
            sender.as_deref(),
            &text,
        )
        .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCommand {
    Help,
    Bind {
        code: String,
    },
    Projects,
    Run {
        project: String,
        instruction: String,
    },
    Sessions {
        project: Option<String>,
    },
    Status {
        session_id: String,
    },
    Cancel {
        session_id: String,
    },
    Continue {
        session_id: String,
        instruction: String,
    },
}

pub fn parse_codex_command(text: &str) -> AppResult<CodexCommand> {
    let cleaned = text
        .replace("\u{a0}", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let Some(rest) = cleaned.strip_prefix("/codex") else {
        return Err(AppError::ValidationFailed);
    };
    let rest = rest.trim();
    if rest.is_empty() || rest == "help" {
        return Ok(CodexCommand::Help);
    }
    let mut parts = rest.splitn(3, ' ');
    let op = parts.next().unwrap_or_default();
    match op {
        "bind" => parts
            .next()
            .filter(|value| !value.is_empty())
            .map(|code| CodexCommand::Bind {
                code: code.to_owned(),
            })
            .ok_or(AppError::ValidationFailed),
        "projects" => Ok(CodexCommand::Projects),
        "run" => {
            let project = parts.next().unwrap_or_default().trim();
            let instruction = parts.next().unwrap_or_default().trim();
            if project.is_empty() || instruction.is_empty() {
                return Err(AppError::ValidationFailed);
            }
            Ok(CodexCommand::Run {
                project: project.to_owned(),
                instruction: instruction.to_owned(),
            })
        }
        "sessions" => Ok(CodexCommand::Sessions {
            project: parts
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }),
        "status" => parts
            .next()
            .filter(|value| !value.is_empty())
            .map(|session_id| CodexCommand::Status {
                session_id: session_id.to_owned(),
            })
            .ok_or(AppError::ValidationFailed),
        "cancel" => parts
            .next()
            .filter(|value| !value.is_empty())
            .map(|session_id| CodexCommand::Cancel {
                session_id: session_id.to_owned(),
            })
            .ok_or(AppError::ValidationFailed),
        "continue" => {
            let session_id = parts.next().unwrap_or_default().trim();
            let instruction = parts.next().unwrap_or_default().trim();
            if session_id.is_empty() || instruction.is_empty() {
                return Err(AppError::ValidationFailed);
            }
            Ok(CodexCommand::Continue {
                session_id: session_id.to_owned(),
                instruction: instruction.to_owned(),
            })
        }
        _ => Err(AppError::ValidationFailed),
    }
}

fn apply_config_input(config: &mut BotConfig, input: &UpsertCollaborationBotInput) {
    if let Some(value) = input
        .app_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        config.app_id = Some(value.to_owned());
    }
    if let Some(value) = input
        .corp_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        config.corp_id = Some(value.to_owned());
    }
    if let Some(value) = input
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        config.agent_id = Some(value.to_owned());
    }
    if let Some(value) = input.callback_public_url.as_deref().map(str::trim) {
        config.callback_public_url = if value.is_empty() {
            None
        } else {
            Some(value.to_owned())
        };
    }
    if let Some(value) = input
        .application_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        config.application_id = Some(value.to_owned());
    }
    if let Some(value) = input.guild_id.as_deref().map(str::trim) {
        config.guild_id = if value.is_empty() {
            None
        } else {
            Some(value.to_owned())
        };
    }
}

fn validate_provider_config(
    provider: CollaborationProvider,
    config: &BotConfig,
    refs: &SecretRefs,
) -> AppResult<()> {
    let ok = match provider {
        CollaborationProvider::Feishu => {
            config.app_id.as_deref().is_some_and(|v| !v.is_empty())
                && refs.contains_key("app_secret")
        }
        CollaborationProvider::Qq => {
            config.app_id.as_deref().is_some_and(|v| !v.is_empty())
                && refs.contains_key("client_secret")
        }
        CollaborationProvider::Wecom => {
            config.corp_id.as_deref().is_some_and(|v| !v.is_empty())
                && config.agent_id.as_deref().is_some_and(|v| !v.is_empty())
                && refs.contains_key("secret")
                && refs.contains_key("token")
                && refs.contains_key("encoding_aes_key")
        }
        CollaborationProvider::Discord => {
            config
                .application_id
                .as_deref()
                .is_some_and(|v| !v.is_empty())
                && refs.contains_key("bot_token")
        }
        CollaborationProvider::Telegram => refs.contains_key("bot_token"),
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::ValidationFailed)
    }
}

fn required_secret_keys(provider: CollaborationProvider) -> &'static [&'static str] {
    match provider {
        CollaborationProvider::Feishu => &["app_secret"],
        CollaborationProvider::Qq => &["client_secret"],
        CollaborationProvider::Wecom => &["secret", "token", "encoding_aes_key"],
        CollaborationProvider::Discord | CollaborationProvider::Telegram => &["bot_token"],
    }
}

fn config_from_json(value: &str) -> AppResult<BotConfig> {
    serde_json::from_str(value).map_err(|_| AppError::Internal)
}

fn secret_refs_from_json(value: &str) -> AppResult<SecretRefs> {
    serde_json::from_str(value).map_err(|_| AppError::Internal)
}

fn credential_mask(provider: CollaborationProvider, config: &BotConfig) -> String {
    match provider {
        CollaborationProvider::Feishu | CollaborationProvider::Qq => config
            .app_id
            .as_deref()
            .map(mask_identifier)
            .unwrap_or_else(|| "••••".to_owned()),
        CollaborationProvider::Wecom => config
            .corp_id
            .as_deref()
            .map(mask_identifier)
            .unwrap_or_else(|| "••••".to_owned()),
        CollaborationProvider::Discord => config
            .application_id
            .as_deref()
            .map(mask_identifier)
            .unwrap_or_else(|| "bot_••••".to_owned()),
        CollaborationProvider::Telegram => "bot_token:••••".to_owned(),
    }
}

fn config_summary(provider: CollaborationProvider, config: &BotConfig) -> String {
    match provider {
        CollaborationProvider::Feishu => "飞书自建应用".to_owned(),
        CollaborationProvider::Qq => "QQ 官方机器人".to_owned(),
        CollaborationProvider::Wecom => format!(
            "企业微信自建应用 · Agent {}",
            config.agent_id.as_deref().unwrap_or("未设置")
        ),
        CollaborationProvider::Discord => config
            .guild_id
            .as_deref()
            .map(|guild| format!("Discord Guild {guild}"))
            .unwrap_or_else(|| "Discord 全局命令".to_owned()),
        CollaborationProvider::Telegram => "Telegram Bot API".to_owned(),
    }
}

fn provider_label(provider: CollaborationProvider) -> &'static str {
    match provider {
        CollaborationProvider::Feishu => "飞书",
        CollaborationProvider::Qq => "QQ",
        CollaborationProvider::Wecom => "企业微信",
        CollaborationProvider::Discord => "Discord",
        CollaborationProvider::Telegram => "Telegram",
    }
}

fn session_card(session: &CodexSessionSummary) -> Value {
    let color = match session.relay_status.as_str() {
        "completed" => "green",
        "failed" => "red",
        "cancelled" => "grey",
        _ => "blue",
    };
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "template": color,
            "title": {"tag": "plain_text", "content": format!("Codex · {}", status_label(&session.relay_status))}
        },
        "elements": [
            {"tag": "div", "text": {"tag": "lark_md", "content": format!("**平台**：{}\n**项目**：{} (`{}`)\n**会话**：`{}`\n**档案**：{}\n**发起人**：{}", provider_label(session.provider), session.project_name, session.project_slug, short_id(&session.id), session.profile_alias, session.started_by.as_deref().unwrap_or("群成员"))}},
            {"tag": "div", "text": {"tag": "lark_md", "content": format!("**摘要**：{}", session.summary.as_deref().unwrap_or("任务正在运行。"))}},
            {"tag": "action", "actions": [
                {"tag": "button", "text": {"tag": "plain_text", "content": "刷新状态"}, "type": "default", "value": {"op": "refresh", "session_id": session.id}},
                {"tag": "button", "text": {"tag": "plain_text", "content": "取消任务"}, "type": "danger", "value": {"op": "cancel", "session_id": session.id}}
            ]}
        ]
    })
}

fn status_label(status: &str) -> &'static str {
    match status {
        "running" => "运行中",
        "completed" => "已完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        _ => "已接收",
    }
}

fn help_text(provider: CollaborationProvider) -> String {
    let prefix = if provider == CollaborationProvider::Discord {
        "Discord 可使用 /codex command:<命令>，文本 fallback 也支持："
    } else {
        "Codex Relay 群命令："
    };
    [
        prefix,
        "/codex projects — 查看当前会话已绑定项目",
        "/codex run <project> <任务说明> — 启动任务",
        "/codex sessions [project] — 查看最近会话",
        "/codex status <session_id> — 查看状态",
        "/codex cancel <session_id> — 取消任务",
        "/codex continue <session_id> <追加说明> — 继续会话",
        "/codex bind <code> — 绑定客户端生成的项目码",
    ]
    .join("\n")
}

fn format_session_status(session: &CodexSessionSummary) -> String {
    format!(
        "Codex · {}\n平台：{}\n项目：{} (`{}`)\n会话：{}\n档案：{}\n摘要：{}",
        status_label(&session.relay_status),
        provider_label(session.provider),
        session.project_name,
        session.project_slug,
        short_id(&session.id),
        session.profile_alias,
        session.summary.as_deref().unwrap_or("任务正在运行。")
    )
}

fn extract_feishu_message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_str()?;
    let parsed: Value = serde_json::from_str(content).ok()?;
    parsed
        .get("text")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_owned())
}

fn normalize_slug(value: &str) -> AppResult<String> {
    let slug = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if slug.is_empty() {
        Err(AppError::ValidationFailed)
    } else {
        Ok(slug)
    }
}

fn generate_bind_code() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(10)
        .map(char::from)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn mask_identifier(value: &str) -> String {
    if value.len() <= 8 {
        return format!("{}••••", value.chars().take(3).collect::<String>());
    }
    format!(
        "{}••••{}",
        value.chars().take(4).collect::<String>(),
        value
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    )
}

fn value_to_id_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
        .unwrap_or_default()
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let raw = xml.split(&start).nth(1)?.split(&end).next()?.trim();
    Some(
        raw.trim_start_matches("<![CDATA[")
            .trim_end_matches("]]>")
            .to_owned(),
    )
}

fn verify_wecom_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    encrypted: &str,
    signature: &str,
) -> AppResult<()> {
    let mut values = [token, timestamp, nonce, encrypted];
    values.sort_unstable();
    let mut hasher = Sha1::new();
    hasher.update(values.join(""));
    let digest = format!("{:x}", hasher.finalize());
    if digest == signature {
        Ok(())
    } else {
        Err(AppError::ValidationFailed)
    }
}

fn decrypt_wecom(aes_key: &str, encrypted: &str) -> AppResult<(String, String)> {
    let key = STANDARD
        .decode(format!("{aes_key}="))
        .map_err(|_| AppError::ValidationFailed)?;
    if key.len() != 32 {
        return Err(AppError::ValidationFailed);
    }
    let mut bytes = STANDARD
        .decode(encrypted)
        .map_err(|_| AppError::ValidationFailed)?;
    let iv = &key[..16];
    let decrypted = Aes256CbcDec::new_from_slices(&key, iv)
        .map_err(|_| AppError::ValidationFailed)?
        .decrypt_padded_mut::<Pkcs7>(&mut bytes)
        .map_err(|_| AppError::ValidationFailed)?;
    if decrypted.len() < 20 {
        return Err(AppError::ValidationFailed);
    }
    let msg_len =
        u32::from_be_bytes([decrypted[16], decrypted[17], decrypted[18], decrypted[19]]) as usize;
    if decrypted.len() < 20 + msg_len {
        return Err(AppError::ValidationFailed);
    }
    let message = String::from_utf8(decrypted[20..20 + msg_len].to_vec())
        .map_err(|_| AppError::ValidationFailed)?;
    let corp = String::from_utf8(decrypted[20 + msg_len..].to_vec()).unwrap_or_default();
    Ok((message, corp))
}

fn write_auth_json_to_home(home: &Path, auth_json: &str) -> AppResult<()> {
    fs::create_dir_all(home).map_err(|_| AppError::RuntimeUnavailable)?;
    let path = home.join("auth.json");
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, auth_json).map_err(|_| AppError::RuntimeUnavailable)?;
    fs::rename(&temporary, &path).map_err(|_| AppError::RuntimeUnavailable)
}

fn parse_codex_session_id(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    for key in ["session_id", "conversation_id", "id"] {
        if let Some(value) = value.get(key).and_then(Value::as_str) {
            if value.len() >= 8 {
                return Some(value.to_owned());
            }
        }
    }
    value
        .pointer("/msg/session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn redact(value: &str) -> String {
    let mut result = value
        .replace("sk-", "[redacted]-")
        .replace("Bearer ", "Bearer [redacted] ");
    if result.chars().count() > 4000 {
        result = truncate_chars(&result, 4000);
    }
    result
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

async fn ensure_success(response: reqwest::Response) -> AppResult<()> {
    if response.status().is_success() || response.status() == HttpStatusCode::OK {
        Ok(())
    } else {
        Err(AppError::UpstreamUnavailable)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(_: reqwest::Error) -> Self {
        AppError::UpstreamUnavailable
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_codex_command, required_secret_keys, CodexCommand, CollaborationManager};
    use crate::{
        database::Repository,
        domain::{CollaborationProvider, UpsertCollaborationBotInput},
        oauth_credentials::OAuthCredentialStore,
        secrets::{MemorySecretStore, SecretStore},
    };
    use std::{path::PathBuf, sync::Arc};

    #[test]
    fn parses_run_command() {
        assert_eq!(
            parse_codex_command("/codex run relay 修复测试").unwrap(),
            CodexCommand::Run {
                project: "relay".into(),
                instruction: "修复测试".into()
            }
        );
    }

    #[test]
    fn parses_continue_command() {
        assert_eq!(
            parse_codex_command("/codex continue abc123 继续处理").unwrap(),
            CodexCommand::Continue {
                session_id: "abc123".into(),
                instruction: "继续处理".into()
            }
        );
    }

    #[test]
    fn rejects_missing_task_body() {
        assert!(parse_codex_command("/codex run relay").is_err());
    }

    #[tokio::test]
    async fn upserts_all_provider_bots_with_masked_metadata() {
        let repository = Arc::new(Repository::memory());
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let manager = CollaborationManager::new(
            repository,
            secrets,
            Arc::new(OAuthCredentialStore::new(
                Arc::new(MemorySecretStore::new()),
            )),
            PathBuf::from("/tmp/codex-relay-collaboration-test"),
        );
        let inputs = vec![
            UpsertCollaborationBotInput {
                id: Some("feishu".into()),
                provider: CollaborationProvider::Feishu,
                name: "Feishu".into(),
                enabled: false,
                confirmed: true,
                app_id: Some("cli_abcdef".into()),
                app_secret: Some("secret-feishu".into()),
                client_secret: None,
                corp_id: None,
                agent_id: None,
                secret: None,
                token: None,
                encoding_aes_key: None,
                callback_public_url: None,
                application_id: None,
                bot_token: None,
                guild_id: None,
            },
            UpsertCollaborationBotInput {
                id: Some("qq".into()),
                provider: CollaborationProvider::Qq,
                name: "QQ".into(),
                enabled: false,
                confirmed: true,
                app_id: Some("qq-app".into()),
                app_secret: None,
                client_secret: Some("secret-qq".into()),
                corp_id: None,
                agent_id: None,
                secret: None,
                token: None,
                encoding_aes_key: None,
                callback_public_url: None,
                application_id: None,
                bot_token: None,
                guild_id: None,
            },
            UpsertCollaborationBotInput {
                id: Some("wecom".into()),
                provider: CollaborationProvider::Wecom,
                name: "WeCom".into(),
                enabled: false,
                confirmed: true,
                app_id: None,
                app_secret: None,
                client_secret: None,
                corp_id: Some("ww123456".into()),
                agent_id: Some("1000002".into()),
                secret: Some("secret-wecom".into()),
                token: Some("token-wecom".into()),
                encoding_aes_key: Some("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into()),
                callback_public_url: Some("https://relay.example.com/callback".into()),
                application_id: None,
                bot_token: None,
                guild_id: None,
            },
            UpsertCollaborationBotInput {
                id: Some("discord".into()),
                provider: CollaborationProvider::Discord,
                name: "Discord".into(),
                enabled: false,
                confirmed: true,
                app_id: None,
                app_secret: None,
                client_secret: None,
                corp_id: None,
                agent_id: None,
                secret: None,
                token: None,
                encoding_aes_key: None,
                callback_public_url: None,
                application_id: Some("123456789".into()),
                bot_token: Some("discord-token".into()),
                guild_id: Some("guild".into()),
            },
            UpsertCollaborationBotInput {
                id: Some("telegram".into()),
                provider: CollaborationProvider::Telegram,
                name: "Telegram".into(),
                enabled: false,
                confirmed: true,
                app_id: None,
                app_secret: None,
                client_secret: None,
                corp_id: None,
                agent_id: None,
                secret: None,
                token: None,
                encoding_aes_key: None,
                callback_public_url: None,
                application_id: None,
                bot_token: Some("123456:telegram-token".into()),
                guild_id: None,
            },
        ];

        for input in inputs {
            let bot = manager.upsert_bot(input).await.unwrap();
            let payload = serde_json::to_string(&bot).unwrap();
            assert!(!payload.contains("secret-"));
            assert!(!payload.contains("discord-token"));
            assert!(!payload.contains("telegram-token"));
            assert_eq!(bot.connection_status, "disabled");
            assert!(!bot.credential_mask.is_empty());
        }

        assert_eq!(manager.list_bots().await.unwrap().len(), 5);
    }

    #[test]
    fn provider_secret_requirements_cover_all_platforms() {
        assert_eq!(
            required_secret_keys(CollaborationProvider::Feishu),
            &["app_secret"]
        );
        assert_eq!(
            required_secret_keys(CollaborationProvider::Qq),
            &["client_secret"]
        );
        assert_eq!(
            required_secret_keys(CollaborationProvider::Discord),
            &["bot_token"]
        );
        assert_eq!(
            required_secret_keys(CollaborationProvider::Telegram),
            &["bot_token"]
        );
        assert_eq!(
            required_secret_keys(CollaborationProvider::Wecom),
            &["secret", "token", "encoding_aes_key"]
        );
    }
}
