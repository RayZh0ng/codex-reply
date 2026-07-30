use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
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
use sha2::Sha256;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use toml_edit::{value as toml_value, Array, DocumentMut, Item, Table};
use uuid::Uuid;

use crate::{
    codex_gateway,
    database::{Repository, StoredCodexSession, StoredCollaborationBot},
    domain::{
        CancelCodexSessionInput, CodexAuthMode, CodexSessionEvent, CodexSessionSummary,
        CollaborationCallbackStatus, CollaborationCommandResult, CollaborationContextSummary,
        CollaborationProjectBinding, CollaborationProvider, ContinueCodexSessionInput,
        DeleteCollaborationBotInput, DeleteCollaborationProjectBindingInput,
        ListCodexSessionsInput, MaskedCollaborationBot, ProfileKind,
        ResetCollaborationContextInput, UpdateCollaborationContextInput,
        UpsertCollaborationBotInput, UpsertCollaborationProjectBindingInput,
    },
    error::{AppError, AppResult},
    gateway::GatewayManager,
    oauth_credentials::{CredentialAccess as OAuthCredentialAccess, OAuthCredentialStore},
    profiles::{CodexOAuthCredential, ImportedAuthFileCredential},
    secrets::SecretStore,
};

const GLOBAL_CONCURRENCY_LIMIT: i64 = 4;
const DEFAULT_PROJECT_CONCURRENCY_LIMIT: i64 = 2;
const CALLBACK_PORT: u16 = 53821;
const COLLABORATION_GATEWAY_PROVIDER: &str = "codex_relay";
const COLLABORATION_MODEL_CATALOG_FILENAME: &str = "codex-relay-model-catalog.json";
const MAX_INCOMING_IMAGES: usize = 5;
const MAX_INCOMING_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const DEFAULT_IMAGE_INSTRUCTION: &str = "请分析这张图片。";

type SecretRefs = HashMap<String, String>;

type Aes256CbcDec = cbc::Decryptor<Aes256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingChatKind {
    Direct,
    Group,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncomingTextIntent {
    Command(String),
    NaturalTask {
        instruction: String,
        force_new: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncomingMessage {
    provider: CollaborationProvider,
    bot_id: String,
    chat_id: String,
    sender: Option<String>,
    text: String,
    chat_kind: IncomingChatKind,
    addressed: bool,
    images: Vec<IncomingImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncomingImage {
    source: IncomingImageSource,
    filename_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncomingImageSource {
    FeishuMessageResource {
        message_id: String,
        file_key: String,
    },
    TelegramFile {
        file_id: String,
    },
    WecomMedia {
        media_id: String,
    },
    Url {
        url: String,
    },
}

enum NaturalRunResolution {
    Ready {
        binding: Box<CollaborationProjectBinding>,
        instruction: String,
        force_new: bool,
    },
    Message(String),
}

#[derive(Debug, Clone)]
enum CommandContextResolution {
    Ready {
        binding: Box<CollaborationProjectBinding>,
        context: Box<CollaborationContextSummary>,
    },
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionExecution {
    target: String,
    model_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionLookupScope {
    provider: CollaborationProvider,
    bot_id: String,
    chat_id: String,
}

#[derive(Clone)]
pub struct CollaborationManager {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    gateway: Arc<GatewayManager>,
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
    system_prompt: Option<String>,
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
        gateway: Arc<GatewayManager>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            repository,
            secrets,
            oauth_credentials,
            gateway,
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
                system_prompt: config.system_prompt.clone(),
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
                "system_prompt": config.system_prompt,
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
                        .header("Authorization", format!("Bot {token}"))
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
        let execution_target = normalize_execution_target(input.execution_target.as_deref())?;
        let model_id = normalized_model_id(input.model_id);
        let bot = self.repository.collaboration_bot(&input.bot_id)?;
        let (profile_id, profile_alias) = if execution_target == "profile" {
            let profile_id = input
                .profile_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(AppError::ValidationFailed)?;
            let profile = self.repository.profile(profile_id)?;
            if !is_codex_runtime_profile(&profile.profile) {
                return Err(AppError::ProfileRuntimeUnavailable);
            }
            (Some(profile.profile.id), Some(profile.profile.alias))
        } else {
            let Some(model) = model_id.as_deref() else {
                return Err(AppError::ValidationFailed);
            };
            validate_gateway_model(&self.repository, model)?;
            (None, None)
        };
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
            profile_id,
            profile_alias,
            chat_id: existing.as_ref().and_then(|item| item.chat_id.clone()),
            bind_code: existing
                .as_ref()
                .map(|item| item.bind_code.clone())
                .unwrap_or_else(generate_bind_code),
            enabled: input.enabled,
            concurrency_limit: input.concurrency_limit.clamp(1, 8),
            execution_target: execution_target.clone(),
            model_id: (execution_target == "gateway")
                .then_some(model_id)
                .flatten(),
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

    pub fn list_contexts(&self) -> AppResult<Vec<CollaborationContextSummary>> {
        self.repository.list_collaboration_contexts()
    }

    pub fn update_context(
        &self,
        input: UpdateCollaborationContextInput,
    ) -> AppResult<CollaborationContextSummary> {
        if !input.confirmed || input.context_id.trim().is_empty() {
            return Err(AppError::ValidationFailed);
        }
        let mut context = self.repository.collaboration_context(&input.context_id)?;
        if let Some(enabled) = input.memory_enabled {
            context = self
                .repository
                .set_collaboration_context_memory(&context.id, enabled)?;
        }
        if let Some(mode) = input
            .conversation_mode
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            context = self
                .repository
                .set_collaboration_context_mode(&context.id, mode)?;
        }
        if let Some(policy) = input.permissions_policy.as_deref() {
            context = self.repository.set_collaboration_context_permissions(
                &context.id,
                normalize_permissions_policy(policy)?,
            )?;
        }
        if input.goal_status.is_some() || input.goal_text.is_some() {
            let status = input
                .goal_status
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(context.goal_status.as_str());
            let text = input
                .goal_text
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            context = self
                .repository
                .set_collaboration_context_goal(&context.id, status, text)?;
        }
        if let Some(model) = input
            .model_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            context = self
                .repository
                .set_collaboration_context_model(&context.id, Some(model))?;
        }
        Ok(context)
    }

    pub fn reset_context(
        &self,
        input: ResetCollaborationContextInput,
    ) -> AppResult<CollaborationContextSummary> {
        if !input.confirmed || input.context_id.trim().is_empty() {
            return Err(AppError::ValidationFailed);
        }
        let context = self
            .repository
            .clear_collaboration_context_active(&input.context_id)?;
        self.repository
            .set_collaboration_context_goal(&context.id, "none", None)
    }

    pub async fn cancel_session(
        &self,
        input: CancelCodexSessionInput,
    ) -> AppResult<CodexSessionSummary> {
        if !input.confirmed {
            return Err(AppError::ConfirmationRequired);
        }
        self.cancel_session_by_reference(&input.session_id, None)
            .await
    }

    pub async fn continue_session(
        &self,
        input: ContinueCodexSessionInput,
    ) -> AppResult<CodexSessionSummary> {
        if !input.confirmed || input.instruction.trim().is_empty() {
            return Err(AppError::ValidationFailed);
        }
        let stored = self.resolve_codex_session_reference(&input.session_id, None)?;
        self.continue_resolved_session(stored, input.instruction, None, Vec::new())
            .await
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
                .header("Authorization", format!("Bot {token}"))
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
        chat_kind: IncomingChatKind,
    ) -> AppResult<()> {
        self.handle_incoming_message(IncomingMessage {
            provider,
            bot_id: bot_id.to_owned(),
            chat_id: chat_id.to_owned(),
            sender: sender.map(ToOwned::to_owned),
            text: text.to_owned(),
            chat_kind,
            addressed: false,
            images: Vec::new(),
        })
        .await
    }

    async fn handle_incoming_message(&self, message: IncomingMessage) -> AppResult<()> {
        let Some(intent) = incoming_message_intent(&message) else {
            return Ok(());
        };
        let result = match intent {
            IncomingTextIntent::Command(command) => {
                self.handle_command_with_images(
                    message.provider,
                    &message.bot_id,
                    &message.chat_id,
                    message.sender.as_deref(),
                    &command,
                    message.images.clone(),
                )
                .await
            }
            IncomingTextIntent::NaturalTask {
                instruction,
                force_new,
            } => {
                self.handle_natural_task(
                    message.provider,
                    &message.bot_id,
                    &message.chat_id,
                    message.sender.as_deref(),
                    instruction,
                    force_new,
                    message.images.clone(),
                )
                .await
            }
        }
        .unwrap_or_else(|error| CollaborationCommandResult {
            status: "failed".to_owned(),
            message: collaboration_error_message(&error),
            session: None,
        });
        if let Some(session) = result.session.as_ref() {
            let _ = self.send_or_update_session_message(session).await;
        } else {
            let _ = self
                .send_platform_text(
                    message.provider,
                    &message.bot_id,
                    &message.chat_id,
                    &result.message,
                )
                .await;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn handle_natural_task(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        sender: Option<&str>,
        instruction: String,
        force_new: bool,
        images: Vec<IncomingImage>,
    ) -> AppResult<CollaborationCommandResult> {
        match self.resolve_natural_run(provider, bot_id, chat_id, instruction, force_new)? {
            NaturalRunResolution::Ready {
                binding,
                instruction,
                force_new,
            } => {
                let binding = *binding;
                let context = self.ensure_context_for_binding(&binding)?;
                self.repository.set_collaboration_chat_context(
                    provider,
                    bot_id,
                    chat_id,
                    &context.id,
                )?;
                self.start_context_turn(
                    binding,
                    context,
                    instruction,
                    sender.map(ToOwned::to_owned),
                    force_new,
                    "natural",
                    "default",
                    images,
                )
                .await
            }
            NaturalRunResolution::Message(message) => Ok(CollaborationCommandResult {
                status: "needs_project".to_owned(),
                message,
                session: None,
            }),
        }
    }

    #[allow(dead_code)]
    pub async fn handle_command(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        sender: Option<&str>,
        text: &str,
    ) -> AppResult<CollaborationCommandResult> {
        self.handle_command_with_images(provider, bot_id, chat_id, sender, text, Vec::new())
            .await
    }

    async fn handle_command_with_images(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        sender: Option<&str>,
        text: &str,
        images: Vec<IncomingImage>,
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
                let context = self.ensure_context_for_binding(&binding)?;
                self.repository.set_collaboration_chat_context(
                    provider,
                    bot_id,
                    chat_id,
                    &context.id,
                )?;
                Ok(CollaborationCommandResult {
                    status: "ok".into(),
                    message: format!(
                        "项目“{}”已绑定到当前会话，并已准备项目全局上下文。",
                        binding.project_name
                    ),
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
                                "- {} ({}) · {}",
                                binding.project_name,
                                binding.project_slug,
                                binding_execution_label(&binding)
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
            CodexCommand::Run { project, instruction } => {
                let binding = self.binding_for_project(provider, bot_id, chat_id, &project)?;
                let context = self.ensure_context_for_binding(&binding)?;
                self.repository.set_collaboration_chat_context(provider, bot_id, chat_id, &context.id)?;
                self.start_context_turn(
                    binding,
                    context,
                    instruction,
                    sender.map(ToOwned::to_owned),
                    false,
                    "run",
                    "default",
                    images,
                )
                .await
            }
            CodexCommand::New { project, instruction } => {
                match self.resolve_command_context(provider, bot_id, chat_id, project.as_deref())? {
                    CommandContextResolution::Ready { binding, context } => {
                        self.start_context_turn(
                            *binding,
                            *context,
                            instruction,
                            sender.map(ToOwned::to_owned),
                            true,
                            "new",
                            "default",
                            images,
                        )
                        .await
                    }
                    CommandContextResolution::Message(message) => Ok(CollaborationCommandResult {
                        status: "needs_project".into(),
                        message,
                        session: None,
                    }),
                }
            }
            CodexCommand::Plan { instruction } => {
                match self.resolve_command_context(provider, bot_id, chat_id, None)? {
                    CommandContextResolution::Ready { binding, context } => {
                        self.start_context_turn(
                            *binding,
                            *context,
                            plan_mode_instruction(&instruction),
                            sender.map(ToOwned::to_owned),
                            false,
                            "plan",
                            "plan",
                            images,
                        )
                        .await
                    }
                    CommandContextResolution::Message(message) => Ok(CollaborationCommandResult {
                        status: "needs_project".into(),
                        message,
                        session: None,
                    }),
                }
            }
            CodexCommand::Goal { action } => self.handle_goal_command(
                provider,
                bot_id,
                chat_id,
                action,
            ),
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
                            "- {} [{} · {}] {}",
                            short_id(&stored.session.id),
                            stored.session.relay_status,
                            stored.session.turn_kind,
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
                if let Some(session_id) = session_id {
                    let scope = SessionLookupScope::new(provider, bot_id, chat_id);
                    let session = self
                        .resolve_codex_session_reference(&session_id, Some(&scope))?
                        .session;
                    Ok(CollaborationCommandResult {
                        status: "ok".into(),
                        message: format_session_status(&session),
                        session: None,
                    })
                } else {
                    let message = match self.resolve_command_context(provider, bot_id, chat_id, None)? {
                        CommandContextResolution::Ready { context, .. } => {
                            format_context_status(&context)
                        }
                        CommandContextResolution::Message(message) => message,
                    };
                    Ok(CollaborationCommandResult {
                        status: "ok".into(),
                        message,
                        session: None,
                    })
                }
            }
            CodexCommand::Cancel { session_id } => {
                let scope = SessionLookupScope::new(provider, bot_id, chat_id);
                self.cancel_session_by_reference(&session_id, Some(&scope))
                    .await
                    .map(|session| CollaborationCommandResult {
                        status: "cancelled".into(),
                        message: "任务已取消。".into(),
                        session: Some(session),
                    })
            }
            CodexCommand::Continue { session_id, instruction } => {
                let scope = SessionLookupScope::new(provider, bot_id, chat_id);
                let stored = self.resolve_codex_session_reference(&session_id, Some(&scope))?;
                self.continue_resolved_session(
                    stored,
                    instruction,
                    sender.map(ToOwned::to_owned),
                    images,
                )
                .await
                .map(|session| CollaborationCommandResult {
                    status: "running".into(),
                    message: "Codex 会话已继续。".into(),
                    session: Some(session),
                })
            }
            CodexCommand::Resume { session_id } => self.resume_context_session(
                provider,
                bot_id,
                chat_id,
                &session_id,
            ),
            CodexCommand::Compact => match self.resolve_command_context(provider, bot_id, chat_id, None)? {
                CommandContextResolution::Ready { binding, context } => {
                    self.start_context_turn(
                        *binding,
                        *context,
                        "请压缩当前协作上下文，保留关键目标、决策、已完成步骤、待办、风险和验证结果，后续继续基于压缩后的上下文工作。".into(),
                        sender.map(ToOwned::to_owned),
                        false,
                        "compact",
                        "compact",
                        images,
                    )
                    .await
                }
                CommandContextResolution::Message(message) => Ok(CollaborationCommandResult { status: "needs_project".into(), message, session: None }),
            },
            CodexCommand::Review => match self.resolve_command_context(provider, bot_id, chat_id, None)? {
                CommandContextResolution::Ready { binding, context } => {
                    self.start_context_turn(
                        *binding,
                        *context,
                        "请审查当前工作区改动，指出风险、缺陷、遗漏的测试和发布阻塞项；不要修改文件。".into(),
                        sender.map(ToOwned::to_owned),
                        false,
                        "review",
                        "review",
                        images,
                    )
                    .await
                }
                CommandContextResolution::Message(message) => Ok(CollaborationCommandResult { status: "needs_project".into(), message, session: None }),
            },
            CodexCommand::Model { model } => self.handle_model_command(provider, bot_id, chat_id, model),
            CodexCommand::Permissions { policy } => {
                self.handle_permissions_command(provider, bot_id, chat_id, policy)
            }
            CodexCommand::Memories { action } => {
                self.handle_memories_command(provider, bot_id, chat_id, action)
            }
        }
    }

    fn resolve_codex_session_reference(
        &self,
        reference: &str,
        scope: Option<&SessionLookupScope>,
    ) -> AppResult<StoredCodexSession> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(AppError::ValidationFailed);
        }
        match self.repository.codex_session(reference) {
            Ok(stored) => {
                if scope.is_none_or(|scope| scope.matches(&stored.session)) {
                    return Ok(stored);
                }
                return Err(AppError::NotFound);
            }
            Err(AppError::NotFound) => {}
            Err(error) => return Err(error),
        }
        if reference.chars().count() < 8 {
            return Err(AppError::ValidationFailed);
        }
        let matches = self
            .repository
            .list_codex_sessions(None)?
            .into_iter()
            .filter(|stored| scope.is_none_or(|scope| scope.matches(&stored.session)))
            .filter(|stored| session_matches_reference(&stored.session, reference))
            .collect::<Vec<_>>();
        match matches.len() {
            0 => Err(AppError::NotFound),
            1 => Ok(matches.into_iter().next().expect("one session match")),
            _ => Err(AppError::Conflict),
        }
    }

    async fn cancel_session_by_reference(
        &self,
        reference: &str,
        scope: Option<&SessionLookupScope>,
    ) -> AppResult<CodexSessionSummary> {
        let stored = self.resolve_codex_session_reference(reference, scope)?;
        self.cancel_session_by_id(&stored.session.id).await
    }

    async fn continue_resolved_session(
        &self,
        stored: StoredCodexSession,
        instruction: String,
        started_by: Option<String>,
        images: Vec<IncomingImage>,
    ) -> AppResult<CodexSessionSummary> {
        let codex_session_id = stored
            .session
            .codex_session_id
            .clone()
            .ok_or(AppError::Conflict)?;
        let mut binding = self
            .repository
            .collaboration_project_binding(&stored.session.binding_id)?;
        apply_stored_session_snapshot(&mut binding, &stored);
        let context = if let Some(context_id) = stored.session.context_id.as_deref() {
            self.repository.collaboration_context(context_id)?
        } else {
            self.ensure_context_for_binding(&binding)?
        };
        self.repository
            .set_collaboration_context_codex_id(&context.id, &codex_session_id)?;
        self.start_context_turn(
            binding,
            context,
            instruction,
            started_by.or(stored.session.started_by),
            false,
            "continue",
            "default",
            images,
        )
        .await
        .map(|result| result.session.expect("continue returns session"))
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

    fn resolve_command_context(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        project: Option<&str>,
    ) -> AppResult<CommandContextResolution> {
        if let Some(project) = project.map(str::trim).filter(|value| !value.is_empty()) {
            let binding = self.binding_for_project(provider, bot_id, chat_id, project)?;
            let context = self.ensure_context_for_binding(&binding)?;
            self.repository.set_collaboration_chat_context(
                provider,
                bot_id,
                chat_id,
                &context.id,
            )?;
            return Ok(CommandContextResolution::Ready {
                binding: Box::new(binding),
                context: Box::new(context),
            });
        }
        if let Some(context) = self
            .repository
            .collaboration_chat_context(provider, bot_id, chat_id)?
        {
            let mut binding = self
                .repository
                .collaboration_project_binding(&context.binding_id)?;
            apply_context_snapshot(&mut binding, &context);
            return Ok(CommandContextResolution::Ready {
                binding: Box::new(binding),
                context: Box::new(context),
            });
        }
        let bindings = self
            .repository
            .collaboration_bindings_for_chat(provider, bot_id, chat_id)?;
        match bindings.as_slice() {
            [] => Ok(CommandContextResolution::Message(
                "当前会话还没有绑定项目。请先在 Codex Relay 客户端创建项目绑定，再发送 /codex bind <code>。"
                    .to_owned(),
            )),
            [binding] => {
                let context = self.ensure_context_for_binding(binding)?;
                self.repository
                    .set_collaboration_chat_context(provider, bot_id, chat_id, &context.id)?;
                Ok(CommandContextResolution::Ready {
                    binding: Box::new(binding.clone()),
                    context: Box::new(context),
                })
            }
            _ => Ok(CommandContextResolution::Message(format!(
                "当前会话绑定了多个项目，请补项目名：\n{}\n可发送 /codex run <project> <任务说明> 指定项目。",
                bindings
                    .into_iter()
                    .map(|binding| format!("- {} ({})", binding.project_name, binding.project_slug))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))),
        }
    }

    fn ensure_context_for_binding(
        &self,
        binding: &CollaborationProjectBinding,
    ) -> AppResult<CollaborationContextSummary> {
        let scope_key = collaboration_context_scope_key(binding);
        if let Some(mut context) = self.repository.collaboration_context_by_scope(&scope_key)? {
            if context.binding_id != binding.id {
                context.binding_id = binding.id.clone();
                context.provider = binding.provider;
                context.bot_id = binding.bot_id.clone();
                context.bot_name = binding.bot_name.clone();
                context.project_name = binding.project_name.clone();
                context.project_slug = binding.project_slug.clone();
                context.updated_at_ms = timestamp_ms();
                self.repository.upsert_collaboration_context(&context)?;
                context = self.repository.collaboration_context(&context.id)?;
            }
            return Ok(context);
        }
        let now = timestamp_ms();
        let context = CollaborationContextSummary {
            id: collaboration_context_id(&scope_key),
            scope_key,
            binding_id: binding.id.clone(),
            provider: binding.provider,
            bot_id: binding.bot_id.clone(),
            bot_name: binding.bot_name.clone(),
            project_name: binding.project_name.clone(),
            project_slug: binding.project_slug.clone(),
            working_directory: binding.working_directory.clone(),
            execution_target: binding.execution_target.clone(),
            profile_id: binding.profile_id.clone(),
            profile_alias: binding.profile_alias.clone(),
            model_id: binding.model_id.clone(),
            memory_enabled: true,
            permissions_policy: "workspace-write".to_owned(),
            active_codex_session_id: None,
            active_relay_session_id: None,
            goal_status: "none".to_owned(),
            goal_text: None,
            conversation_mode: "default".to_owned(),
            last_turn_at_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.repository.upsert_collaboration_context(&context)?;
        Ok(context)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_context_turn(
        &self,
        binding: CollaborationProjectBinding,
        mut context: CollaborationContextSummary,
        instruction: String,
        started_by: Option<String>,
        force_new: bool,
        turn_kind: &str,
        conversation_mode: &str,
        images: Vec<IncomingImage>,
    ) -> AppResult<CollaborationCommandResult> {
        if self
            .repository
            .count_running_codex_sessions_for_context(&context.id)?
            > 0
        {
            return Ok(CollaborationCommandResult {
                status: "limited".into(),
                message: format!(
                    "项目“{}”已有一个上下文 turn 正在运行，请先等待完成或使用 /codex status 查看。",
                    context.project_name
                ),
                session: None,
            });
        }
        if force_new {
            context = self
                .repository
                .clear_collaboration_context_active(&context.id)?;
        }
        let codex_session_id = context.active_codex_session_id.clone();
        self.start_codex_session(
            binding,
            build_context_instruction(&context, &instruction, conversation_mode),
            started_by,
            codex_session_id.is_some(),
            codex_session_id,
            None,
            Some(context),
            turn_kind,
            conversation_mode,
            images,
        )
        .await
    }

    fn handle_goal_command(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        action: GoalAction,
    ) -> AppResult<CollaborationCommandResult> {
        let context = match self.resolve_command_context(provider, bot_id, chat_id, None)? {
            CommandContextResolution::Ready { context, .. } => *context,
            CommandContextResolution::Message(message) => {
                return Ok(CollaborationCommandResult {
                    status: "needs_project".into(),
                    message,
                    session: None,
                });
            }
        };
        let context = match action {
            GoalAction::View => context,
            GoalAction::Set(text) | GoalAction::Edit(text) => self
                .repository
                .set_collaboration_context_goal(&context.id, "active", Some(text.trim()))?,
            GoalAction::Pause => self.repository.set_collaboration_context_goal(
                &context.id,
                "paused",
                context.goal_text.as_deref(),
            )?,
            GoalAction::Resume => self.repository.set_collaboration_context_goal(
                &context.id,
                "active",
                context.goal_text.as_deref(),
            )?,
            GoalAction::Clear => {
                self.repository
                    .set_collaboration_context_goal(&context.id, "none", None)?
            }
        };
        Ok(CollaborationCommandResult {
            status: "ok".into(),
            message: format_context_goal_status(&context),
            session: None,
        })
    }

    fn handle_model_command(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        model: Option<String>,
    ) -> AppResult<CollaborationCommandResult> {
        let context = match self.resolve_command_context(provider, bot_id, chat_id, None)? {
            CommandContextResolution::Ready { context, .. } => *context,
            CommandContextResolution::Message(message) => {
                return Ok(CollaborationCommandResult {
                    status: "needs_project".into(),
                    message,
                    session: None,
                });
            }
        };
        let context = if let Some(model) = model
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            self.repository
                .set_collaboration_context_model(&context.id, Some(&model))?
        } else {
            context
        };
        Ok(CollaborationCommandResult {
            status: "ok".into(),
            message: format!(
                "当前模型：{}",
                context
                    .model_id
                    .as_deref()
                    .unwrap_or("跟随 Codex 档案默认模型")
            ),
            session: None,
        })
    }

    fn handle_permissions_command(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        policy: Option<String>,
    ) -> AppResult<CollaborationCommandResult> {
        let context = match self.resolve_command_context(provider, bot_id, chat_id, None)? {
            CommandContextResolution::Ready { context, .. } => *context,
            CommandContextResolution::Message(message) => {
                return Ok(CollaborationCommandResult {
                    status: "needs_project".into(),
                    message,
                    session: None,
                });
            }
        };
        let context = if let Some(policy) = policy {
            let policy = normalize_permissions_policy(&policy)?;
            self.repository
                .set_collaboration_context_permissions(&context.id, policy)?
        } else {
            context
        };
        Ok(CollaborationCommandResult {
            status: "ok".into(),
            message: format!("当前权限策略：{}", context.permissions_policy),
            session: None,
        })
    }

    fn handle_memories_command(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        action: Option<String>,
    ) -> AppResult<CollaborationCommandResult> {
        let context = match self.resolve_command_context(provider, bot_id, chat_id, None)? {
            CommandContextResolution::Ready { context, .. } => *context,
            CommandContextResolution::Message(message) => {
                return Ok(CollaborationCommandResult {
                    status: "needs_project".into(),
                    message,
                    session: None,
                });
            }
        };
        let context = match action.as_deref() {
            Some("on" | "enable" | "enabled") => self
                .repository
                .set_collaboration_context_memory(&context.id, true)?,
            Some("off" | "disable" | "disabled") => self
                .repository
                .set_collaboration_context_memory(&context.id, false)?,
            Some("status") | None => context,
            _ => return Err(AppError::ValidationFailed),
        };
        Ok(CollaborationCommandResult {
            status: "ok".into(),
            message: format!(
                "长期记忆：{}",
                if context.memory_enabled {
                    "已开启"
                } else {
                    "已关闭"
                }
            ),
            session: None,
        })
    }

    fn resume_context_session(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        reference: &str,
    ) -> AppResult<CollaborationCommandResult> {
        let scope = SessionLookupScope::new(provider, bot_id, chat_id);
        let stored = self.resolve_codex_session_reference(reference, Some(&scope))?;
        let codex_session_id = stored
            .session
            .codex_session_id
            .clone()
            .ok_or(AppError::Conflict)?;
        let binding = self
            .repository
            .collaboration_project_binding(&stored.session.binding_id)?;
        let context = if let Some(context_id) = stored.session.context_id.as_deref() {
            self.repository.collaboration_context(context_id)?
        } else {
            self.ensure_context_for_binding(&binding)?
        };
        self.repository
            .set_collaboration_context_codex_id(&context.id, &codex_session_id)?;
        self.repository
            .set_collaboration_chat_context(provider, bot_id, chat_id, &context.id)?;
        Ok(CollaborationCommandResult {
            status: "ok".into(),
            message: format!(
                "已切换当前上下文到 Codex session {}。下一条自然消息会继续该上下文。",
                short_id(&codex_session_id)
            ),
            session: None,
        })
    }

    fn resolve_natural_run(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        instruction: String,
        force_new: bool,
    ) -> AppResult<NaturalRunResolution> {
        let bindings = self
            .repository
            .collaboration_bindings_for_chat(provider, bot_id, chat_id)?;
        if bindings.is_empty() {
            return Ok(NaturalRunResolution::Message(
                "当前会话还没有绑定项目。请先在 Codex Relay 客户端创建项目绑定，再发送 /codex bind <code>。"
                    .to_owned(),
            ));
        }
        if let Some(context) = self
            .repository
            .collaboration_chat_context(provider, bot_id, chat_id)?
        {
            if let Some(binding) = bindings
                .iter()
                .find(|binding| binding.id == context.binding_id && binding.enabled)
            {
                return Ok(NaturalRunResolution::Ready {
                    binding: Box::new(binding.clone()),
                    instruction,
                    force_new,
                });
            }
        }
        match bindings.as_slice() {
            [binding] => Ok(NaturalRunResolution::Ready {
                binding: Box::new(binding.clone()),
                instruction,
                force_new,
            }),
            _ => Ok(NaturalRunResolution::Message(format!(
                "当前会话绑定了多个项目，请补项目名：\n{}\n可发送 /codex run <project> <任务说明> 或 /new <project> <任务说明> 指定项目。",
                bindings
                    .into_iter()
                    .map(|binding| format!("- {} ({})", binding.project_name, binding.project_slug))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))),
        }
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
        if let Some(context_id) = session.context_id.as_deref() {
            let _ = self
                .repository
                .mark_collaboration_context_turn_finished(context_id, session_id);
        }
        self.record_event(session_id, "cancelled", "任务已取消。");
        let manager = self.clone();
        let session_for_update = session.clone();
        tauri::async_runtime::spawn(async move {
            let _ = manager.update_session_message(&session_for_update).await;
        });
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_codex_session(
        &self,
        binding: CollaborationProjectBinding,
        instruction: String,
        started_by: Option<String>,
        resume: bool,
        codex_session_id: Option<String>,
        execution_override: Option<SessionExecution>,
        context: Option<CollaborationContextSummary>,
        turn_kind: &str,
        conversation_mode: &str,
        images: Vec<IncomingImage>,
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
        if let Some(context) = context.as_ref() {
            if self
                .repository
                .count_running_codex_sessions_for_context(&context.id)?
                > 0
            {
                return Ok(CollaborationCommandResult {
                    status: "limited".into(),
                    message: format!(
                        "项目“{}”已有一个上下文 turn 正在运行，请稍后查看状态。",
                        context.project_name
                    ),
                    session: None,
                });
            }
        }
        let execution = execution_override.unwrap_or_else(|| execution_from_binding(&binding));
        let working_directory = Path::new(&binding.working_directory);
        if !working_directory.is_dir() {
            return Err(AppError::RuntimeUnavailable);
        }
        let session_id = Uuid::new_v4().to_string();
        let now = timestamp_ms();
        let context_id = context.as_ref().map(|context| context.id.clone());
        let session_home = context_id
            .as_deref()
            .map(|context_id| self.context_home(context_id))
            .unwrap_or_else(|| self.session_home(&session_id));
        let run_dir = context_id
            .as_deref()
            .map(|context_id| self.context_turn_dir(context_id, &session_id))
            .unwrap_or_else(|| self.session_home(&session_id));
        if execution.target == "gateway" {
            let auth_json = self.gateway_oauth_auth_json().await?;
            let model = execution
                .model_id
                .as_deref()
                .ok_or(AppError::ValidationFailed)?;
            self.write_gateway_session_config(&session_home, model, auth_json.as_deref())
                .await?;
        } else {
            let profile_id = binding
                .profile_id
                .as_deref()
                .ok_or(AppError::ProfileRuntimeUnavailable)?;
            let profile = self.repository.profile(profile_id)?;
            let auth_json = self.profile_auth_json(&profile).await?;
            write_auth_json_to_home(&session_home, &auth_json)?;
        }
        if let Some(context) = context.as_ref() {
            write_context_memory_config(&session_home, context.memory_enabled)?;
        }
        let image_paths = match self
            .download_incoming_images(&binding.provider, &binding.bot_id, &run_dir, &images)
            .await
        {
            Ok(paths) => paths,
            Err(message) => {
                return Ok(CollaborationCommandResult {
                    status: "failed".into(),
                    message,
                    session: None,
                });
            }
        };
        fs::create_dir_all(&run_dir).map_err(|_| AppError::RuntimeUnavailable)?;
        let output_file = run_dir.join("last-message.txt");
        let mut command = Command::new("codex");
        let args = codex_command_args(
            resume,
            codex_session_id.as_deref(),
            &output_file,
            &image_paths,
            context
                .as_ref()
                .and_then(|context| context.model_id.as_deref()),
            context
                .as_ref()
                .map(|context| context.permissions_policy.as_str())
                .unwrap_or("workspace-write"),
        )?;
        command.args(args);
        let system_prompt = self.bot_system_prompt(&binding.bot_id)?;
        let codex_instruction =
            build_codex_instruction(system_prompt.as_deref(), &instruction, image_paths.len());
        command
            .env("CODEX_HOME", &session_home)
            .env_remove("CODEX_API_KEY")
            .current_dir(working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                AppError::CodexCliMissing
            } else {
                AppError::RuntimeUnavailable
            }
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(codex_instruction.as_bytes())
                .and_then(|()| stdin.flush())
                .map_err(|_| AppError::RuntimeUnavailable)?;
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let session = CodexSessionSummary {
            id: session_id.clone(),
            binding_id: binding.id.clone(),
            context_id: context_id.clone(),
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
            execution_target: execution.target.clone(),
            model_id: context
                .as_ref()
                .and_then(|context| context.model_id.clone())
                .or_else(|| execution.model_id.clone()),
            turn_kind: turn_kind.to_owned(),
            conversation_mode: conversation_mode.to_owned(),
            goal_status: context.as_ref().map(|context| context.goal_status.clone()),
        };
        self.repository.insert_codex_session(&StoredCodexSession {
            session: session.clone(),
            working_directory: binding.working_directory.clone(),
        })?;
        if let Some(context_id) = context_id.as_deref() {
            self.repository
                .mark_collaboration_context_turn_started(context_id, &session_id)?;
        }
        self.record_event(&session_id, "received", &redact(&instruction));
        if !image_paths.is_empty() {
            self.record_event(
                &session_id,
                "attachments",
                &format!("已附加 {} 张图片。", image_paths.len()),
            );
        }
        let child = Arc::new(Mutex::new(child));
        self.active
            .lock()
            .map_err(|_| AppError::Internal)?
            .insert(session_id.clone(), child.clone());
        self.spawn_session_watcher(
            session_id.clone(),
            context_id,
            child,
            stdout,
            stderr,
            output_file,
        );
        Ok(CollaborationCommandResult {
            status: "running".into(),
            message: "Codex 任务已启动。".into(),
            session: Some(session),
        })
    }

    fn spawn_session_watcher(
        &self,
        session_id: String,
        context_id: Option<String>,
        child: Arc<Mutex<Child>>,
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
        output_file: PathBuf,
    ) {
        let repository = self.repository.clone();
        let active = self.active.clone();
        let manager = self.clone();
        thread::spawn(move || {
            let mut stderr_tail = VecDeque::new();
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
                        if let Some(context_id) = context_id.as_deref() {
                            let _ = repository
                                .set_collaboration_context_codex_id(context_id, &codex_id);
                        }
                    }
                }
            }
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        let redacted = redact(&line);
                        stderr_tail.push_back(redacted.clone());
                        while stderr_tail.len() > 12 {
                            stderr_tail.pop_front();
                        }
                        let _ = repository.insert_codex_session_event(&CodexSessionEvent {
                            id: Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            occurred_at_ms: timestamp_ms(),
                            event_type: "stderr".into(),
                            content: redacted,
                        });
                    }
                }
            }
            let exit_status = child.lock().ok().and_then(|mut child| child.wait().ok());
            let success = exit_status.as_ref().is_some_and(|status| status.success());
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
            if repository
                .codex_session(&session_id)
                .ok()
                .is_some_and(|stored| stored.session.relay_status == "cancelled")
            {
                if let Some(context_id) = context_id.as_deref() {
                    let _ = repository
                        .mark_collaboration_context_turn_finished(context_id, &session_id);
                }
                let _ = repository.insert_codex_session_event(&CodexSessionEvent {
                    id: Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    occurred_at_ms: timestamp_ms(),
                    event_type: "cancelled_ack".into(),
                    content: "Codex 进程已在取消后退出。".into(),
                });
                return;
            }
            let status = if success { "completed" } else { "failed" };
            let last_error = (!success)
                .then(|| codex_failure_detail(exit_status.as_ref(), &stderr_tail, &final_summary));
            let session = repository.set_codex_session_status(
                &session_id,
                status,
                Some(&truncate_chars(&final_summary, 1500)),
                last_error.as_deref(),
                Some(timestamp_ms()),
            );
            let _ = repository.insert_codex_session_event(&CodexSessionEvent {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                occurred_at_ms: timestamp_ms(),
                event_type: status.into(),
                content: final_summary,
            });
            if let Some(context_id) = context_id.as_deref() {
                let _ =
                    repository.mark_collaboration_context_turn_finished(context_id, &session_id);
            }
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

    async fn write_gateway_session_config(
        &self,
        home: &Path,
        model: &str,
        auth_json: Option<&str>,
    ) -> AppResult<()> {
        let gateway = self.gateway.status()?;
        if !gateway.running || !gateway.certificate_ready {
            return Err(AppError::GatewayNotRunning);
        }
        if gateway.available_profiles == 0 {
            return Err(AppError::UpstreamUnavailable);
        }
        let secret_ref =
            codex_gateway::ensure_codex_client_key(&self.repository, self.secrets.clone()).await?;
        write_gateway_session_files_to_home(
            home,
            auth_json,
            &gateway.service_url,
            model,
            &secret_ref,
            &self.data_dir,
        )
    }

    async fn gateway_oauth_auth_json(&self) -> AppResult<Option<String>> {
        codex_gateway::codex_oauth_profile_auth_json(
            &self.repository,
            &self.oauth_credentials,
            OAuthCredentialAccess::Background,
        )
        .await
    }

    fn session_home(&self, session_id: &str) -> PathBuf {
        self.data_dir
            .join("collaboration-sessions")
            .join(session_id)
            .join("codex-home")
    }

    fn context_home(&self, context_id: &str) -> PathBuf {
        self.data_dir
            .join("collaboration-contexts")
            .join(context_id)
            .join("codex-home")
    }

    fn context_turn_dir(&self, context_id: &str, session_id: &str) -> PathBuf {
        self.data_dir
            .join("collaboration-contexts")
            .join(context_id)
            .join("turns")
            .join(session_id)
    }

    fn bot_system_prompt(&self, bot_id: &str) -> AppResult<Option<String>> {
        let stored = self.repository.collaboration_bot(bot_id)?;
        config_from_json(&stored.config_json).map(|config| config.system_prompt)
    }

    async fn download_incoming_images(
        &self,
        provider: &CollaborationProvider,
        bot_id: &str,
        run_dir: &Path,
        images: &[IncomingImage],
    ) -> Result<Vec<PathBuf>, String> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        if images.len() > MAX_INCOMING_IMAGES {
            return Err(format!(
                "图片数量超过上限：最多支持 {MAX_INCOMING_IMAGES} 张，本次收到 {} 张。",
                images.len()
            ));
        }
        let directory = run_dir.join("incoming-images");
        fs::create_dir_all(&directory).map_err(|_| "图片保存目录创建失败。".to_owned())?;
        let mut paths = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let bytes = self
                .download_incoming_image(provider, bot_id, image)
                .await
                .map_err(|reason| format!("第 {} 张图片下载失败：{reason}", index + 1))?;
            if bytes.is_empty() {
                return Err(format!("第 {} 张图片为空。", index + 1));
            }
            if bytes.len() as u64 > MAX_INCOMING_IMAGE_BYTES {
                return Err(format!(
                    "第 {} 张图片超过 20MB 上限（{} 字节）。",
                    index + 1,
                    bytes.len()
                ));
            }
            let extension = image_extension_from_hint(image.filename_hint.as_deref())
                .unwrap_or_else(|| image_extension_from_bytes(&bytes));
            let path = directory.join(format!("image-{}.{}", index + 1, extension));
            fs::write(&path, bytes).map_err(|_| format!("第 {} 张图片保存失败。", index + 1))?;
            paths.push(path);
        }
        Ok(paths)
    }

    async fn download_incoming_image(
        &self,
        _provider: &CollaborationProvider,
        bot_id: &str,
        image: &IncomingImage,
    ) -> Result<Vec<u8>, String> {
        match &image.source {
            IncomingImageSource::FeishuMessageResource {
                message_id,
                file_key,
            } => {
                let client = self
                    .feishu_client(bot_id)
                    .await
                    .map_err(|error| collaboration_error_message(&error))?;
                let file = client
                    .im_v1_message_resource()
                    .download(
                        message_id.clone(),
                        file_key.clone(),
                        vec![("type".to_owned(), "image".to_owned())],
                        feishu_sdk::core::RequestOptions::default(),
                    )
                    .await
                    .map_err(|error| format!("飞书资源接口返回异常：{error}"))?;
                Ok(file.bytes)
            }
            IncomingImageSource::TelegramFile { file_id } => {
                let runtime = self
                    .runtime(bot_id)
                    .await
                    .map_err(|error| collaboration_error_message(&error))?;
                let token = self
                    .secret(&runtime, "bot_token")
                    .await
                    .map_err(|error| collaboration_error_message(&error))?;
                let file: Value = Client::new()
                    .post(format!("https://api.telegram.org/bot{token}/getFile"))
                    .json(&json!({"file_id": file_id}))
                    .send()
                    .await
                    .map_err(|_| "Telegram getFile 请求失败。".to_owned())?
                    .json()
                    .await
                    .map_err(|_| "Telegram getFile 响应无法解析。".to_owned())?;
                let file_path = file
                    .get("result")
                    .and_then(|result| result.get("file_path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Telegram 未返回 file_path。".to_owned())?;
                download_url_limited(
                    &format!("https://api.telegram.org/file/bot{token}/{file_path}"),
                    None,
                )
                .await
            }
            IncomingImageSource::WecomMedia { media_id } => {
                let runtime = self
                    .runtime(bot_id)
                    .await
                    .map_err(|error| collaboration_error_message(&error))?;
                let token = self
                    .wecom_access_token(&runtime)
                    .await
                    .map_err(|error| collaboration_error_message(&error))?;
                let url = format!(
                    "https://qyapi.weixin.qq.com/cgi-bin/media/get?access_token={token}&media_id={media_id}"
                );
                download_url_limited(&url, None).await
            }
            IncomingImageSource::Url { url } => download_url_limited(url, None).await,
        }
    }

    fn record_event(&self, session_id: &str, event_type: &str, content: &str) {
        if let Err(error) = self
            .repository
            .insert_codex_session_event(&CodexSessionEvent {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.to_owned(),
                occurred_at_ms: timestamp_ms(),
                event_type: event_type.to_owned(),
                content: content.to_owned(),
            })
        {
            eprintln!(
                "[collaboration] failed to record codex session event {event_type} for {session_id}: {error}"
            );
        }
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
        let message_id = match session.provider {
            CollaborationProvider::Feishu => {
                self.send_feishu_card(bot_id, chat_id, session).await?
            }
            CollaborationProvider::Telegram => {
                self.send_telegram_session_message(bot_id, chat_id, session)
                    .await?
            }
            CollaborationProvider::Discord => {
                self.send_discord_session_message(bot_id, chat_id, session)
                    .await?
            }
            _ => {
                self.send_platform_text(
                    session.provider,
                    bot_id,
                    chat_id,
                    &format_session_status(session),
                )
                .await?
            }
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
                self.edit_telegram_session_message(bot_id, chat_id, message_id, session)
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
                self.edit_discord_session_message(bot_id, chat_id, message_id, session)
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
        if let Some((op, session_id)) = session_action_from_value(&value) {
            self.handle_unscoped_session_action(op, session_id).await?;
        }
        Ok(())
    }

    async fn handle_unscoped_session_action(&self, op: &str, session_id: &str) -> AppResult<()> {
        match op {
            "cancel" => {
                let session = self.cancel_session_by_id(session_id).await?;
                self.update_session_message(&session).await?;
            }
            "refresh" => {
                let session = self.repository.codex_session(session_id)?.session;
                self.update_session_message(&session).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_scoped_session_action(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        data: &str,
    ) -> AppResult<()> {
        let Some((op, session_id)) = parse_session_action_data(data) else {
            return Ok(());
        };
        let scope = SessionLookupScope::new(provider, bot_id, chat_id);
        match op {
            "cancel" => {
                let session = self
                    .cancel_session_by_reference(session_id, Some(&scope))
                    .await?;
                self.update_session_message(&session).await?;
            }
            "refresh" => {
                let session = self
                    .resolve_codex_session_reference(session_id, Some(&scope))?
                    .session;
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
            .map_err(|_| AppError::UpstreamUnavailable)?
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
            .map_err(|_| AppError::UpstreamUnavailable)?
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
            .map_err(|_| AppError::UpstreamUnavailable)?
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
                    if let Some(callback) = update.get("callback_query") {
                        let data = callback
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let chat_id = callback
                            .get("message")
                            .and_then(|message| message.get("chat"))
                            .and_then(|chat| chat.get("id"))
                            .map(value_to_id_string)
                            .unwrap_or_default();
                        if !chat_id.is_empty() {
                            let _ = self.answer_telegram_callback(&token, callback).await;
                            let _ = self
                                .handle_scoped_session_action(
                                    CollaborationProvider::Telegram,
                                    &bot_id,
                                    &chat_id,
                                    data,
                                )
                                .await;
                        }
                        continue;
                    }
                    let Some(message) =
                        update.get("message").or_else(|| update.get("channel_post"))
                    else {
                        continue;
                    };
                    let text = message
                        .get("text")
                        .or_else(|| message.get("caption"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let chat_id = message
                        .get("chat")
                        .and_then(|chat| chat.get("id"))
                        .map(value_to_id_string)
                        .unwrap_or_default();
                    let chat_kind = if message
                        .get("chat")
                        .and_then(|chat| chat.get("type"))
                        .and_then(Value::as_str)
                        == Some("private")
                    {
                        IncomingChatKind::Direct
                    } else {
                        IncomingChatKind::Group
                    };
                    let sender = message
                        .get("from")
                        .and_then(|from| from.get("username").or_else(|| from.get("id")))
                        .map(value_to_id_string);
                    if !chat_id.is_empty() {
                        let _ = self
                            .handle_incoming_message(IncomingMessage {
                                provider: CollaborationProvider::Telegram,
                                bot_id: bot_id.clone(),
                                chat_id,
                                sender,
                                text: text.to_owned(),
                                chat_kind,
                                addressed: false,
                                images: telegram_images(message),
                            })
                            .await;
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn answer_telegram_callback(&self, token: &str, callback: &Value) -> AppResult<()> {
        let Some(callback_query_id) = callback.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        ensure_success(
            Client::new()
                .post(format!(
                    "https://api.telegram.org/bot{token}/answerCallbackQuery"
                ))
                .json(&json!({"callback_query_id": callback_query_id}))
                .send()
                .await?,
        )
        .await
    }

    async fn send_telegram_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
    ) -> AppResult<String> {
        self.send_telegram_message_with_markup(bot_id, chat_id, text, None)
            .await
    }

    async fn send_telegram_session_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        session: &CodexSessionSummary,
    ) -> AppResult<String> {
        self.send_telegram_message_with_markup(
            bot_id,
            chat_id,
            &format_session_status(session),
            Some(telegram_session_reply_markup(session)),
        )
        .await
    }

    async fn send_telegram_message_with_markup(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
        reply_markup: Option<Value>,
    ) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        let mut body = json!({"chat_id": chat_id, "text": truncate_chars(text, 3900)});
        if let Some(reply_markup) = reply_markup {
            body["reply_markup"] = reply_markup;
        }
        let response: Value = Client::new()
            .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
            .json(&body)
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

    async fn edit_telegram_session_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        session: &CodexSessionSummary,
    ) -> AppResult<()> {
        self.edit_telegram_message_with_markup(
            bot_id,
            chat_id,
            message_id,
            &format_session_status(session),
            Some(telegram_session_reply_markup(session)),
        )
        .await
    }

    #[allow(dead_code)]
    async fn edit_telegram_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> AppResult<()> {
        self.edit_telegram_message_with_markup(bot_id, chat_id, message_id, text, None)
            .await
    }

    async fn edit_telegram_message_with_markup(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
        reply_markup: Option<Value>,
    ) -> AppResult<()> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        let mut body = json!({"chat_id": chat_id, "message_id": message_id, "text": truncate_chars(text, 3900)});
        if let Some(reply_markup) = reply_markup {
            body["reply_markup"] = reply_markup;
        }
        ensure_success(
            Client::new()
                .post(format!(
                    "https://api.telegram.org/bot{token}/editMessageText"
                ))
                .json(&body)
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
            .header("Authorization", format!("Bot {token}"))
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
                    let chat_kind = if d.get("guild_id").is_some_and(|value| !value.is_null()) {
                        IncomingChatKind::Group
                    } else {
                        IncomingChatKind::Direct
                    };
                    let sender = d
                        .get("author")
                        .and_then(|a| a.get("username").or_else(|| a.get("id")))
                        .map(value_to_id_string);
                    let _ = self
                        .handle_incoming_message(IncomingMessage {
                            provider: CollaborationProvider::Discord,
                            bot_id: bot_id.clone(),
                            chat_id: chat_id.to_owned(),
                            sender,
                            text: text.to_owned(),
                            chat_kind,
                            addressed: false,
                            images: discord_images(d),
                        })
                        .await;
                }
                "INTERACTION_CREATE" => {
                    let d = payload.get("d").unwrap_or(&Value::Null);
                    if let Some(custom_id) = d
                        .get("data")
                        .and_then(|data| data.get("custom_id"))
                        .and_then(Value::as_str)
                    {
                        let chat_id = d.get("channel_id").and_then(Value::as_str).unwrap_or_default();
                        self.ack_discord_component_interaction(d).await.ok();
                        let _ = self
                            .handle_scoped_session_action(
                                CollaborationProvider::Discord,
                                &bot_id,
                                chat_id,
                                custom_id,
                            )
                            .await;
                        continue;
                    }
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
                    let chat_kind = if d.get("guild_id").is_some_and(|value| !value.is_null()) {
                        IncomingChatKind::Group
                    } else {
                        IncomingChatKind::Direct
                    };
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
                            chat_kind,
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

    async fn ack_discord_component_interaction(&self, interaction: &Value) -> AppResult<()> {
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
                .post(format!(
                    "https://discord.com/api/v10/interactions/{id}/{token}/callback"
                ))
                .json(&json!({"type": 6}))
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
        self.send_discord_message_with_components(bot_id, chat_id, text, None)
            .await
    }

    async fn send_discord_session_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        session: &CodexSessionSummary,
    ) -> AppResult<String> {
        self.send_discord_message_with_components(
            bot_id,
            chat_id,
            &format_session_status(session),
            Some(discord_session_components(session)),
        )
        .await
    }

    async fn send_discord_message_with_components(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
        components: Option<Value>,
    ) -> AppResult<String> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        let mut body = json!({"content": truncate_chars(text, 1900)});
        if let Some(components) = components {
            body["components"] = components;
        }
        let response: Value = Client::new()
            .post(format!(
                "https://discord.com/api/v10/channels/{chat_id}/messages"
            ))
            .header("Authorization", format!("Bot {token}"))
            .json(&body)
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

    async fn edit_discord_session_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        session: &CodexSessionSummary,
    ) -> AppResult<()> {
        self.edit_discord_message_with_components(
            bot_id,
            chat_id,
            message_id,
            &format_session_status(session),
            Some(discord_session_components(session)),
        )
        .await
    }

    #[allow(dead_code)]
    async fn edit_discord_message(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> AppResult<()> {
        self.edit_discord_message_with_components(bot_id, chat_id, message_id, text, None)
            .await
    }

    async fn edit_discord_message_with_components(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
        components: Option<Value>,
    ) -> AppResult<()> {
        let runtime = self.runtime(bot_id).await?;
        let token = self.secret(&runtime, "bot_token").await?;
        let mut body = json!({"content": truncate_chars(text, 1900)});
        if let Some(components) = components {
            body["components"] = components;
        }
        ensure_success(
            Client::new()
                .patch(format!(
                    "https://discord.com/api/v10/channels/{chat_id}/messages/{message_id}"
                ))
                .header("Authorization", format!("Bot {token}"))
                .json(&body)
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
                        let chat_kind = if matches!(event_type, "GROUP_AT_MESSAGE_CREATE" | "AT_MESSAGE_CREATE") {
                            IncomingChatKind::Group
                        } else {
                            IncomingChatKind::Direct
                        };
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
                            .handle_incoming_message(IncomingMessage {
                                provider: CollaborationProvider::Qq,
                                bot_id: bot_id.clone(),
                                chat_id,
                                sender,
                                text: text.to_owned(),
                                chat_kind,
                                addressed: matches!(event_type, "GROUP_AT_MESSAGE_CREATE" | "AT_MESSAGE_CREATE"),
                                images: qq_images(d),
                            })
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
        let msg_type = xml_tag(&xml, "MsgType").unwrap_or_else(|| "text".to_owned());
        let content = xml_tag(&xml, "Content").unwrap_or_default();
        let from = xml_tag(&xml, "FromUserName").unwrap_or_else(|| "unknown".to_owned());
        let chat_id_from_group = xml_tag(&xml, "ChatId");
        let chat_kind = if chat_id_from_group.is_some() {
            IncomingChatKind::Group
        } else {
            IncomingChatKind::Direct
        };
        let chat_id = chat_id_from_group
            .or_else(|| xml_tag(&xml, "FromUserName"))
            .unwrap_or_default();
        if !chat_id.is_empty() {
            self.handle_incoming_message(IncomingMessage {
                provider: CollaborationProvider::Wecom,
                bot_id: bot_id.to_owned(),
                chat_id,
                sender: Some(from),
                text: content,
                chat_kind,
                addressed: false,
                images: wecom_images_from_xml(&xml, &msg_type),
            })
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
        let message_id = message
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (text, images) = extract_feishu_message_parts(message, message_id);
        let chat_kind = if message.get("chat_type").and_then(Value::as_str) == Some("p2p") {
            IncomingChatKind::Direct
        } else {
            IncomingChatKind::Group
        };
        let addressed = message
            .get("mentions")
            .and_then(Value::as_array)
            .is_some_and(|mentions| !mentions.is_empty());
        self.handle_incoming_message(IncomingMessage {
            provider: CollaborationProvider::Feishu,
            bot_id: bot_id.to_owned(),
            chat_id,
            sender,
            text,
            chat_kind,
            addressed,
            images,
        })
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
    New {
        project: Option<String>,
        instruction: String,
    },
    Plan {
        instruction: String,
    },
    Goal {
        action: GoalAction,
    },
    Sessions {
        project: Option<String>,
    },
    Status {
        session_id: Option<String>,
    },
    Cancel {
        session_id: String,
    },
    Continue {
        session_id: String,
        instruction: String,
    },
    Resume {
        session_id: String,
    },
    Compact,
    Review,
    Model {
        model: Option<String>,
    },
    Permissions {
        policy: Option<String>,
    },
    Memories {
        action: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    View,
    Set(String),
    Edit(String),
    Pause,
    Resume,
    Clear,
}

pub fn parse_codex_command(text: &str) -> AppResult<CodexCommand> {
    let cleaned = text
        .replace("\u{a0}", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let rest = command_body(&cleaned).ok_or(AppError::ValidationFailed)?;
    if rest.is_empty() || rest == "help" {
        return Ok(CodexCommand::Help);
    }
    let mut parts = rest.splitn(2, ' ');
    let op = parts.next().unwrap_or_default();
    let tail = parts.next().unwrap_or_default().trim();
    match op {
        "bind" => tail
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
            .map(|code| CodexCommand::Bind {
                code: code.to_owned(),
            })
            .ok_or(AppError::ValidationFailed),
        "projects" => Ok(CodexCommand::Projects),
        "run" => {
            let (project, instruction) = split_required_project_instruction(tail)?;
            Ok(CodexCommand::Run {
                project,
                instruction,
            })
        }
        "new" => {
            let (project, instruction) = split_optional_project_instruction(tail)?;
            Ok(CodexCommand::New {
                project,
                instruction,
            })
        }
        "plan" => {
            if tail.is_empty() {
                return Err(AppError::ValidationFailed);
            }
            Ok(CodexCommand::Plan {
                instruction: tail.to_owned(),
            })
        }
        "goal" => Ok(CodexCommand::Goal {
            action: parse_goal_action(tail)?,
        }),
        "sessions" => Ok(CodexCommand::Sessions {
            project: tail
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }),
        "status" => Ok(CodexCommand::Status {
            session_id: tail
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }),
        "cancel" => tail
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
            .map(|session_id| CodexCommand::Cancel {
                session_id: session_id.to_owned(),
            })
            .ok_or(AppError::ValidationFailed),
        "continue" => {
            let (session_id, instruction) = split_required_project_instruction(tail)?;
            Ok(CodexCommand::Continue {
                session_id,
                instruction,
            })
        }
        "resume" => tail
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
            .map(|session_id| CodexCommand::Resume {
                session_id: session_id.to_owned(),
            })
            .ok_or(AppError::ValidationFailed),
        "compact" => Ok(CodexCommand::Compact),
        "review" => Ok(CodexCommand::Review),
        "model" => Ok(CodexCommand::Model {
            model: tail
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }),
        "permissions" => Ok(CodexCommand::Permissions {
            policy: tail
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }),
        "memories" => Ok(CodexCommand::Memories {
            action: tail
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(|value| value.to_ascii_lowercase()),
        }),
        _ => Err(AppError::ValidationFailed),
    }
}

fn command_body(cleaned: &str) -> Option<&str> {
    if let Some(rest) = cleaned.strip_prefix("/codex") {
        let rest = rest.trim_start();
        if let Some(encoded) = rest.strip_prefix("command:") {
            return Some(encoded.trim_matches('"').trim());
        }
        return Some(rest.trim());
    }
    cleaned.strip_prefix('/').map(str::trim)
}

fn split_required_project_instruction(text: &str) -> AppResult<(String, String)> {
    let mut parts = text.splitn(2, ' ');
    let project = parts.next().unwrap_or_default().trim();
    let instruction = parts.next().unwrap_or_default().trim();
    if project.is_empty() || instruction.is_empty() {
        return Err(AppError::ValidationFailed);
    }
    Ok((project.to_owned(), instruction.to_owned()))
}

fn split_optional_project_instruction(text: &str) -> AppResult<(Option<String>, String)> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::ValidationFailed);
    }
    let mut parts = text.splitn(2, ' ');
    let first = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();
    if rest.is_empty() {
        Ok((None, first.to_owned()))
    } else {
        Ok((Some(first.to_owned()), rest.to_owned()))
    }
}

fn parse_goal_action(text: &str) -> AppResult<GoalAction> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(GoalAction::View);
    }
    let mut parts = text.splitn(2, ' ');
    let op = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match op {
        "edit" => {
            if rest.is_empty() {
                Err(AppError::ValidationFailed)
            } else {
                Ok(GoalAction::Edit(rest.to_owned()))
            }
        }
        "pause" => Ok(GoalAction::Pause),
        "resume" => Ok(GoalAction::Resume),
        "clear" => Ok(GoalAction::Clear),
        _ => Ok(GoalAction::Set(text.to_owned())),
    }
}

#[allow(dead_code)]
fn incoming_text_intent(text: &str, chat_kind: IncomingChatKind) -> Option<IncomingTextIntent> {
    incoming_message_intent(&IncomingMessage {
        provider: CollaborationProvider::Feishu,
        bot_id: String::new(),
        chat_id: String::new(),
        sender: None,
        text: text.to_owned(),
        chat_kind,
        addressed: false,
        images: Vec::new(),
    })
}

fn incoming_message_intent(message: &IncomingMessage) -> Option<IncomingTextIntent> {
    let text = &message.text;
    let chat_kind = message.chat_kind;
    let cleaned = text.replace("\u{a0}", " ");
    let trimmed = cleaned.trim();
    let has_images = !message.images.is_empty();
    if trimmed.is_empty() && !has_images {
        return None;
    }
    if trimmed.starts_with("/codex")
        || (message.chat_kind == IncomingChatKind::Direct && trimmed.starts_with('/'))
    {
        return Some(IncomingTextIntent::Command(trimmed.to_owned()));
    }

    let (mentioned, after_mentions) = strip_leading_platform_mentions(trimmed);
    let mentioned = mentioned || message.addressed;
    let after_mentions = after_mentions.trim();
    if mentioned && after_mentions.starts_with('/') {
        return Some(IncomingTextIntent::Command(after_mentions.to_owned()));
    }

    let natural = match chat_kind {
        IncomingChatKind::Direct => {
            if mentioned && !after_mentions.is_empty() {
                after_mentions
            } else if trimmed.is_empty() && has_images {
                DEFAULT_IMAGE_INSTRUCTION
            } else {
                trimmed
            }
        }
        IncomingChatKind::Group if mentioned && !after_mentions.is_empty() => after_mentions,
        IncomingChatKind::Group if mentioned && has_images => DEFAULT_IMAGE_INSTRUCTION,
        IncomingChatKind::Group => return None,
    };
    let (force_new, instruction) = strip_force_new_prefix(natural);
    if instruction.is_empty() {
        None
    } else if !force_new && is_natural_projects_query(&instruction) {
        Some(IncomingTextIntent::Command("/codex projects".to_owned()))
    } else {
        Some(IncomingTextIntent::NaturalTask {
            instruction,
            force_new,
        })
    }
}

fn is_natural_projects_query(text: &str) -> bool {
    let normalized = text
        .chars()
        .filter(|character| {
            !character.is_whitespace()
                && !matches!(
                    character,
                    '?' | '？' | '!' | '！' | ',' | '，' | '.' | '。' | ':' | '：'
                )
        })
        .collect::<String>();
    if normalized.is_empty() {
        return false;
    }
    let lower = normalized.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "projects"
            | "projectlist"
            | "listprojects"
            | "showprojects"
            | "boundprojects"
            | "bindings"
            | "projectbindings"
    ) {
        return true;
    }
    if matches!(
        normalized.as_str(),
        "项目列表" | "项目清单" | "绑定项目" | "已绑定项目" | "当前项目" | "当前绑定项目"
    ) {
        return true;
    }
    let has_project = normalized.contains("项目");
    let has_binding = normalized.contains("绑定") || normalized.contains('绑');
    let has_query = [
        "什么",
        "哪些",
        "哪个",
        "几个",
        "多少",
        "列表",
        "清单",
        "看看",
        "查看",
        "看下",
        "看一下",
        "当前",
        "现在",
        "已",
        "有",
    ]
    .iter()
    .any(|marker| normalized.contains(marker));
    has_project && has_binding && has_query
}

fn strip_leading_platform_mentions(mut text: &str) -> (bool, &str) {
    let mut stripped = false;
    loop {
        let current = text.trim_start();
        if let Some(rest) = strip_xml_at_mention(current) {
            text = rest;
            stripped = true;
            continue;
        }
        if let Some(rest) = strip_angle_mention(current) {
            text = rest;
            stripped = true;
            continue;
        }
        if let Some(rest) = strip_cq_at_mention(current) {
            text = rest;
            stripped = true;
            continue;
        }
        if let Some(rest) = strip_at_token(current) {
            text = rest;
            stripped = true;
            continue;
        }
        return (stripped, current);
    }
}

fn strip_xml_at_mention(text: &str) -> Option<&str> {
    if !text.starts_with("<at") {
        return None;
    }
    text.find("</at>")
        .map(|index| &text[index + "</at>".len()..])
}

fn strip_angle_mention(text: &str) -> Option<&str> {
    if !(text.starts_with("<@") || text.starts_with("<at:")) {
        return None;
    }
    text.find('>').map(|index| &text[index + 1..])
}

fn strip_cq_at_mention(text: &str) -> Option<&str> {
    if !text.starts_with("[CQ:at,") {
        return None;
    }
    text.find(']').map(|index| &text[index + 1..])
}

fn strip_at_token(text: &str) -> Option<&str> {
    if !text.starts_with('@') {
        return None;
    }
    for (index, character) in text.char_indices().skip(1) {
        if character.is_whitespace() {
            return Some(&text[index..]);
        }
        if matches!(character, ':' | '：' | ',' | '，') {
            return Some(&text[index + character.len_utf8()..]);
        }
    }
    Some("")
}

fn strip_force_new_prefix(text: &str) -> (bool, String) {
    let trimmed = text.trim();
    for prefix in ["新任务", "新会话"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return (true, trim_task_prefix_separator(rest).to_owned());
        }
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("new task") {
        let rest = &trimmed["new task".len()..];
        return (true, trim_task_prefix_separator(rest).to_owned());
    }
    (false, trimmed.to_owned())
}

fn trim_task_prefix_separator(text: &str) -> &str {
    text.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, ':' | '：' | '-' | '—' | ',' | '，')
    })
    .trim()
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
    if let Some(value) = input.system_prompt.as_deref().map(str::trim) {
        config.system_prompt = if value.is_empty() {
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

fn collaboration_context_scope_key(binding: &CollaborationProjectBinding) -> String {
    let directory = fs::canonicalize(&binding.working_directory)
        .unwrap_or_else(|_| PathBuf::from(&binding.working_directory))
        .display()
        .to_string();
    format!(
        "dir={directory}|target={}|profile={}|model={}",
        binding.execution_target,
        binding.profile_id.as_deref().unwrap_or(""),
        binding.model_id.as_deref().unwrap_or("")
    )
}

fn collaboration_context_id(scope_key: &str) -> String {
    let digest = Sha256::digest(scope_key.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("ctx-{hex}")
}

fn apply_context_snapshot(
    binding: &mut CollaborationProjectBinding,
    context: &CollaborationContextSummary,
) {
    binding.profile_id = context.profile_id.clone();
    binding.profile_alias = context.profile_alias.clone();
    binding.model_id = context.model_id.clone();
}

fn normalize_permissions_policy(policy: &str) -> AppResult<&'static str> {
    match policy.trim().to_ascii_lowercase().as_str() {
        "read-only" | "readonly" | "ro" => Ok("read-only"),
        "workspace-write" | "workspace" | "write" | "auto" => Ok("workspace-write"),
        "danger-full-access" | "danger" | "full" | "full-access" => Ok("danger-full-access"),
        _ => Err(AppError::ValidationFailed),
    }
}

fn write_context_memory_config(home: &Path, enabled: bool) -> AppResult<()> {
    fs::create_dir_all(home).map_err(|_| AppError::RuntimeUnavailable)?;
    let path = home.join("config.toml");
    let mut document = fs::read_to_string(&path)
        .ok()
        .and_then(|content| content.parse::<DocumentMut>().ok())
        .unwrap_or_default();
    let features = document["features"].or_insert(Item::Table(Table::new()));
    let features = features.as_table_like_mut().ok_or(AppError::Internal)?;
    features.insert("goals", toml_value(true));
    features.insert("memories", toml_value(true));
    let memories = document["memories"].or_insert(Item::Table(Table::new()));
    let memories = memories.as_table_like_mut().ok_or(AppError::Internal)?;
    memories.insert("use_memories", toml_value(enabled));
    memories.insert("generate_memories", toml_value(enabled));
    fs::write(path, document.to_string()).map_err(|_| AppError::RuntimeUnavailable)
}

fn build_context_instruction(
    context: &CollaborationContextSummary,
    instruction: &str,
    conversation_mode: &str,
) -> String {
    let mut parts = Vec::new();
    if context.memory_enabled {
        parts.push(
            "协作上下文：启用 Codex memories；请复用该项目上下文中的长期偏好、决策和历史结论。"
                .to_owned(),
        );
    }
    if context.goal_status == "active" {
        if let Some(goal) = context
            .goal_text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            parts.push(format!("当前长期目标：\n{goal}"));
        }
    }
    match conversation_mode {
        "plan" => parts.push("当前 turn 使用计划模式：先理解现状、提出可执行计划；除非用户要求实现，否则不要改文件。".to_owned()),
        "compact" => parts.push("当前 turn 用于压缩上下文：输出可供后续继续执行的短摘要。".to_owned()),
        "review" => parts.push("当前 turn 用于审查：聚焦风险、缺陷和测试缺口。".to_owned()),
        _ => {}
    }
    parts.push(format!("用户任务：\n{}", instruction.trim()));
    parts.join("\n\n")
}

fn plan_mode_instruction(instruction: &str) -> String {
    format!(
        "请进入 Codex 计划模式，为以下任务提出决策完整的执行计划。任务：\n{}",
        instruction.trim()
    )
}

fn format_context_status(context: &CollaborationContextSummary) -> String {
    format!(
        "Codex Relay 上下文 · {}\n项目：{} (`{}`)\n执行：{}\n记忆：{}\n权限：{}\n目标：{}\n当前 Codex session：{}\n当前 Relay turn：{}",
        context.conversation_mode,
        context.project_name,
        context.project_slug,
        context_execution_label(context),
        if context.memory_enabled { "开启" } else { "关闭" },
        context.permissions_policy,
        context_goal_label(context),
        context
            .active_codex_session_id
            .as_deref()
            .map(short_id)
            .unwrap_or_else(|| "暂无".to_owned()),
        context
            .active_relay_session_id
            .as_deref()
            .map(short_id)
            .unwrap_or_else(|| "空闲".to_owned()),
    )
}

fn format_context_goal_status(context: &CollaborationContextSummary) -> String {
    format!("长期目标：{}", context_goal_label(context))
}

fn context_goal_label(context: &CollaborationContextSummary) -> String {
    match context.goal_status.as_str() {
        "active" => context
            .goal_text
            .as_deref()
            .map(|text| format!("进行中 · {text}"))
            .unwrap_or_else(|| "进行中".to_owned()),
        "paused" => context
            .goal_text
            .as_deref()
            .map(|text| format!("已暂停 · {text}"))
            .unwrap_or_else(|| "已暂停".to_owned()),
        _ => "未设置".to_owned(),
    }
}

fn context_execution_label(context: &CollaborationContextSummary) -> String {
    match context.execution_target.as_str() {
        "gateway" => context
            .model_id
            .as_deref()
            .map(|model| format!("API 网关 · {model}"))
            .unwrap_or_else(|| "API 网关".to_owned()),
        _ => context
            .model_id
            .as_deref()
            .map(|model| format!("档案直连 · {model}"))
            .unwrap_or_else(|| "档案直连".to_owned()),
    }
}

fn binding_execution_label(binding: &CollaborationProjectBinding) -> String {
    match binding.execution_target.as_str() {
        "gateway" => binding
            .model_id
            .as_deref()
            .map(|model| format!("API 网关 · {model}"))
            .unwrap_or_else(|| "API 网关".to_owned()),
        _ => "档案直连".to_owned(),
    }
}

impl SessionLookupScope {
    fn new(provider: CollaborationProvider, bot_id: &str, chat_id: &str) -> Self {
        Self {
            provider,
            bot_id: bot_id.to_owned(),
            chat_id: chat_id.to_owned(),
        }
    }

    fn matches(&self, session: &CodexSessionSummary) -> bool {
        session.provider == self.provider
            && session.provider_bot_id.as_deref() == Some(self.bot_id.as_str())
            && session.provider_chat_id.as_deref() == Some(self.chat_id.as_str())
    }
}

fn session_matches_reference(session: &CodexSessionSummary, reference: &str) -> bool {
    session.id == reference
        || session.id.starts_with(reference)
        || session.codex_session_id.as_deref() == Some(reference)
        || session
            .codex_session_id
            .as_deref()
            .is_some_and(|codex_id| codex_id.starts_with(reference))
}

fn session_action_from_value(value: &Value) -> Option<(&str, &str)> {
    let op = value
        .get("op")
        .or_else(|| value.get("action"))
        .and_then(Value::as_str)?;
    let session_id = value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)?;
    if op.is_empty() || session_id.is_empty() {
        None
    } else {
        Some((op, session_id))
    }
}

fn parse_session_action_data(data: &str) -> Option<(&str, &str)> {
    let data = data.trim();
    let rest = data.strip_prefix("codex:")?;
    let mut parts = rest.splitn(2, ':');
    let op = parts.next()?.trim();
    let session_id = parts.next()?.trim();
    if op.is_empty() || session_id.is_empty() {
        None
    } else {
        Some((op, session_id))
    }
}

fn execution_from_binding(binding: &CollaborationProjectBinding) -> SessionExecution {
    SessionExecution {
        target: binding.execution_target.clone(),
        model_id: binding.model_id.clone(),
    }
}

fn apply_session_profile_snapshot(
    binding: &mut CollaborationProjectBinding,
    session: &CodexSessionSummary,
) {
    binding.profile_id = session.profile_id.clone();
    binding.profile_alias = session.profile_alias.clone();
}

fn apply_stored_session_snapshot(
    binding: &mut CollaborationProjectBinding,
    stored: &StoredCodexSession,
) {
    apply_session_profile_snapshot(binding, &stored.session);
    binding.working_directory = stored.working_directory.clone();
}

fn write_gateway_config_to_home(
    home: &Path,
    service_url: &str,
    model: &str,
    secret_ref: &str,
    data_dir: &Path,
) -> AppResult<()> {
    fs::create_dir_all(home).map_err(|_| AppError::RuntimeUnavailable)?;
    let mut document = DocumentMut::new();
    document["model_provider"] = toml_value(COLLABORATION_GATEWAY_PROVIDER);
    document["model"] = toml_value(model);
    document["model_catalog_json"] = toml_value(COLLABORATION_MODEL_CATALOG_FILENAME);
    let providers = document["model_providers"].or_insert(Item::Table(Table::new()));
    let providers = providers
        .as_table_like_mut()
        .ok_or(AppError::ValidationFailed)?;
    let mut provider = Table::new();
    provider["name"] = toml_value("Codex Relay API Gateway");
    provider["base_url"] = toml_value(format!("{}/v1", service_url));
    provider["wire_api"] = toml_value("responses");
    let mut auth = Table::new();
    auth["command"] = toml_value(
        std::env::current_exe()
            .map_err(|_| AppError::RuntimeUnavailable)?
            .display()
            .to_string(),
    );
    let mut args = Array::new();
    args.push("--relay-gateway-token");
    args.push(secret_ref);
    args.push("--relay-data-dir");
    args.push(data_dir.display().to_string());
    auth["args"] = Item::Value(args.into());
    provider["auth"] = Item::Table(auth);
    providers.insert(COLLABORATION_GATEWAY_PROVIDER, Item::Table(provider));
    fs::write(home.join("config.toml"), document.to_string())
        .map_err(|_| AppError::RuntimeUnavailable)?;
    write_gateway_model_catalog(home, model)
}

fn write_gateway_session_files_to_home(
    home: &Path,
    auth_json: Option<&str>,
    service_url: &str,
    model: &str,
    secret_ref: &str,
    data_dir: &Path,
) -> AppResult<()> {
    if let Some(auth_json) = auth_json {
        write_auth_json_to_home(home, auth_json)?;
    }
    write_gateway_config_to_home(home, service_url, model, secret_ref, data_dir)
}

fn write_gateway_model_catalog(home: &Path, model: &str) -> AppResult<()> {
    let catalog = json!({
        "models": [{
            "slug": model,
            "display_name": model,
            "description": model,
            "base_instructions": "You are Codex, a coding agent. You and the user share the same workspace and collaborate to achieve the user's goals.",
            "default_reasoning_level": "high",
            "supported_reasoning_levels": [
                {"effort": "none", "description": "Disable Thinking"},
                {"effort": "high", "description": "Enabled Thinking"}
            ],
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 1000,
            "supports_reasoning_summaries": true,
            "context_window": 128000
        }]
    });
    let text = serde_json::to_string_pretty(&catalog).map_err(|_| AppError::Internal)?;
    fs::write(home.join(COLLABORATION_MODEL_CATALOG_FILENAME), text)
        .map_err(|_| AppError::RuntimeUnavailable)
}

fn codex_command_args(
    resume: bool,
    codex_session_id: Option<&str>,
    output_file: &Path,
    images: &[PathBuf],
    model: Option<&str>,
    permissions_policy: &str,
) -> AppResult<Vec<String>> {
    let mut args = vec!["exec".to_owned()];
    if resume {
        args.push("resume".to_owned());
    }
    args.push("--json".to_owned());
    args.push("--output-last-message".to_owned());
    args.push(output_file.display().to_string());
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    if resume {
        args.push("-c".to_owned());
        args.push(format!("sandbox_mode=\"{}\"", permissions_policy));
    } else {
        args.push("--sandbox".to_owned());
        args.push(permissions_policy.to_owned());
    }
    push_image_args(&mut args, images);
    if resume {
        args.push(
            codex_session_id
                .ok_or(AppError::ValidationFailed)?
                .to_owned(),
        );
    }
    args.push("-".to_owned());
    Ok(args)
}

fn push_image_args(args: &mut Vec<String>, images: &[PathBuf]) {
    for image in images {
        args.push("--image".to_owned());
        args.push(image.display().to_string());
    }
}

fn build_codex_instruction(
    system_prompt: Option<&str>,
    instruction: &str,
    image_count: usize,
) -> String {
    let system_prompt = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if system_prompt.is_none() && image_count == 0 {
        return instruction.to_owned();
    }
    let mut parts = Vec::new();
    if let Some(prompt) = system_prompt {
        parts.push(format!("机器人专属提示词：\n{prompt}"));
    }
    parts.push(format!("用户任务：\n{}", instruction.trim()));
    if image_count > 0 {
        parts.push(format!(
            "图片附件：本次消息已附加 {image_count} 张图片，请结合图片内容完成用户任务。"
        ));
    }
    parts.join("\n\n")
}

async fn download_url_limited(url: &str, authorization: Option<&str>) -> Result<Vec<u8>, String> {
    let mut request = Client::new().get(url);
    if let Some(authorization) = authorization {
        request = request.header("Authorization", authorization);
    }
    let response = request
        .send()
        .await
        .map_err(|_| "网络请求失败。".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "下载接口返回 HTTP {}。",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_INCOMING_IMAGE_BYTES)
    {
        return Err("图片超过 20MB 上限。".to_owned());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "图片响应读取失败。".to_owned())?;
    if bytes.len() as u64 > MAX_INCOMING_IMAGE_BYTES {
        return Err("图片超过 20MB 上限。".to_owned());
    }
    Ok(bytes.to_vec())
}

fn image_extension_from_hint(hint: Option<&str>) -> Option<String> {
    let hint = hint?.trim().to_ascii_lowercase();
    for extension in [
        "png", "jpg", "jpeg", "webp", "gif", "bmp", "ico", "tiff", "heic",
    ] {
        if hint.ends_with(&format!(".{extension}")) || hint == extension {
            return Some(
                if extension == "jpeg" {
                    "jpg"
                } else {
                    extension
                }
                .to_owned(),
            );
        }
    }
    None
}

fn image_extension_from_bytes(bytes: &[u8]) -> String {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png".to_owned()
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg".to_owned()
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif".to_owned()
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "webp".to_owned()
    } else if bytes.starts_with(b"BM") {
        "bmp".to_owned()
    } else {
        "png".to_owned()
    }
}

fn normalize_execution_target(value: Option<&str>) -> AppResult<String> {
    match value.unwrap_or("profile").trim() {
        "" | "profile" => Ok("profile".to_owned()),
        "gateway" => Ok("gateway".to_owned()),
        _ => Err(AppError::ValidationFailed),
    }
}

fn normalized_model_id(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn validate_gateway_model(repository: &Repository, model: &str) -> AppResult<()> {
    if codex_gateway::gateway_model_is_available(repository, model)? {
        Ok(())
    } else {
        Err(AppError::GatewayModelUnavailable)
    }
}

fn is_codex_runtime_profile(profile: &crate::domain::MaskedProfile) -> bool {
    profile.kind == ProfileKind::CodexOauth && profile.enabled && profile.credential_configured
}

fn session_card(session: &CodexSessionSummary) -> Value {
    let color = match session.relay_status.as_str() {
        "completed" => "green",
        "failed" => "red",
        "cancelled" => "grey",
        _ => "blue",
    };
    let mut actions = vec![json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": "刷新状态"},
        "type": "default",
        "value": {"op": "refresh", "session_id": session.id},
    })];
    if session.relay_status == "running" {
        actions.push(json!({
            "tag": "button",
            "text": {"tag": "plain_text", "content": "取消任务"},
            "type": "danger",
            "value": {"op": "cancel", "session_id": session.id},
        }));
    }
    let mut elements = vec![
        json!({"tag": "div", "text": {"tag": "lark_md", "content": format!("**平台**：{}\n**项目**：{} (`{}`)\n**会话**：`{}`\n**档案**：{}\n**发起人**：{}", provider_label(session.provider), session.project_name, session.project_slug, short_id(&session.id), profile_alias_label(session.profile_alias.as_deref()), session.started_by.as_deref().unwrap_or("群成员"))}}),
        json!({"tag": "div", "text": {"tag": "lark_md", "content": format!("**摘要**：{}", session.summary.as_deref().unwrap_or("任务正在运行。"))}}),
    ];
    if session.relay_status == "failed" {
        elements.push(json!({"tag": "div", "text": {"tag": "lark_md", "content": format!("**失败原因**：{}", truncate_chars(session.last_error.as_deref().unwrap_or("未记录详细原因。"), 1200))}}));
    }
    elements.push(json!({"tag": "action", "actions": actions}));
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "template": color,
            "title": {"tag": "plain_text", "content": format!("Codex · {}", status_label(&session.relay_status))}
        },
        "elements": elements
    })
}

fn telegram_session_reply_markup(session: &CodexSessionSummary) -> Value {
    let mut buttons = vec![json!({
        "text": "刷新状态",
        "callback_data": format!("codex:refresh:{}", session.id),
    })];
    if session.relay_status == "running" {
        buttons.push(json!({
            "text": "取消任务",
            "callback_data": format!("codex:cancel:{}", session.id),
        }));
    }
    json!({"inline_keyboard": [buttons]})
}

fn discord_session_components(session: &CodexSessionSummary) -> Value {
    let mut components = vec![json!({
        "type": 2,
        "style": 2,
        "label": "刷新状态",
        "custom_id": format!("codex:refresh:{}", session.id),
    })];
    if session.relay_status == "running" {
        components.push(json!({
            "type": 2,
            "style": 4,
            "label": "取消任务",
            "custom_id": format!("codex:cancel:{}", session.id),
        }));
    }
    json!([{"type": 1, "components": components}])
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
        "/codex bind <code> — 绑定客户端生成的项目码",
        "/codex projects — 查看当前会话已绑定项目",
        "@机器人 <自然语言> — 首次创建项目共享上下文，后续续接同一 Codex context",
        "/codex new [project] <任务说明> — 创建并切换新的 Codex 会话",
        "/codex plan <任务说明> — 使用计划模式执行一个 turn",
        "/codex goal <objective|edit|pause|resume|clear> — 设置或管理长期目标",
        "/codex memories on|off|status — 开关或查看长期记忆",
        "/codex model [model] — 查看或切换上下文默认模型",
        "/codex permissions [read-only|workspace-write|danger-full-access] — 查看或切换权限策略",
        "/codex status [session_id] — 查看上下文或会话状态",
        "/codex resume <session_id> — 切换 active Codex session",
        "/codex compact — 压缩当前上下文",
        "/codex review — 审查当前项目改动",
        "/codex sessions [project] — 查看最近会话",
        "/codex cancel <session_id> — 取消任务",
        "/codex continue <session_id> <追加说明> — 兼容旧式继续会话",
    ]
    .join("\n")
}

fn collaboration_error_message(error: &AppError) -> String {
    let action = match error {
        AppError::ProfileRuntimeUnavailable => {
            "档案暂不能用于 Codex 会话，请完成 OAuth 授权或重新选择执行档案。"
        }
        AppError::GatewayNotRunning => "网关尚未运行，请先启动 Relay 网关并确认 HTTPS 证书就绪。",
        AppError::GatewayModelUnavailable => {
            "选择的网关模型当前不可用，请刷新模型并确认账号已加入网关账号池。"
        }
        AppError::RuntimeUnavailable => {
            "本机运行时或项目目录不可用，请确认工作目录存在且 Codex CLI 可启动。"
        }
        AppError::CodexCliMissing => {
            "未检测到 Codex CLI，请在设置页运行环境检查并安装 @openai/codex。"
        }
        AppError::LocalStateUnavailable => {
            "本机会话状态暂不可读，请在 Codex Relay 客户端协作页刷新机器人连接；若仍出现，请重启 Codex Relay 并保留该错误码。"
        }
        AppError::UpstreamUnavailable => "上游服务当前不可用，请检查网关账号池、网络与凭据。",
        AppError::Conflict => "会话短 ID 出现冲突，请使用完整会话 ID 后重试。",
        AppError::NotFound => "未找到匹配的项目或会话，请检查绑定状态与会话 ID。",
        AppError::ValidationFailed => "命令参数不完整，请检查 /codex 命令格式。",
        _ => return format!("{}（错误码：{}）", error, error.code()),
    };
    format!(
        "{action}
错误码：{}",
        error.code()
    )
}

fn format_session_status(session: &CodexSessionSummary) -> String {
    let mut message = format!(
        "Codex · {}\n平台：{}\n项目：{} (`{}`)\n会话：{}\n执行：{}\n档案：{}\n摘要：{}",
        status_label(&session.relay_status),
        provider_label(session.provider),
        session.project_name,
        session.project_slug,
        short_id(&session.id),
        execution_label(session),
        profile_alias_label(session.profile_alias.as_deref()),
        session.summary.as_deref().unwrap_or("任务正在运行。")
    );
    if session.relay_status == "failed" {
        message.push_str("\n失败原因：");
        message.push_str(&truncate_chars(
            session.last_error.as_deref().unwrap_or("未记录详细原因。"),
            1200,
        ));
    }
    if session.relay_status == "running" {
        message.push_str(&format!(
            "\n操作：/codex status {} · /codex cancel {}",
            short_id(&session.id),
            short_id(&session.id)
        ));
    } else {
        message.push_str(&format!("\n操作：/codex status {}", short_id(&session.id)));
    }
    message
}

fn execution_label(session: &CodexSessionSummary) -> String {
    match session.execution_target.as_str() {
        "gateway" => session
            .model_id
            .as_deref()
            .map(|model| format!("API 网关 · {model}"))
            .unwrap_or_else(|| "API 网关".to_owned()),
        _ => "档案直连".to_owned(),
    }
}

fn profile_alias_label(alias: Option<&str>) -> &str {
    alias
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("网关全局设置")
}

fn extract_feishu_message_parts(message: &Value, message_id: &str) -> (String, Vec<IncomingImage>) {
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let parsed: Value = serde_json::from_str(content).unwrap_or(Value::Null);
    let mut texts = Vec::new();
    let mut image_keys = Vec::new();
    collect_feishu_text_and_images(&parsed, &mut texts, &mut image_keys);
    let text = texts
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned();
    let images = image_keys
        .into_iter()
        .filter(|key| !key.trim().is_empty() && !message_id.is_empty())
        .map(|file_key| IncomingImage {
            source: IncomingImageSource::FeishuMessageResource {
                message_id: message_id.to_owned(),
                file_key,
            },
            filename_hint: Some("feishu-image.png".to_owned()),
        })
        .collect();
    (text, images)
}

fn collect_feishu_text_and_images(
    value: &Value,
    texts: &mut Vec<String>,
    image_keys: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("text" | "un_escape_text", Value::String(text)) => texts.push(text.clone()),
                    ("image_key" | "file_key", Value::String(image_key)) => {
                        image_keys.push(image_key.clone())
                    }
                    _ => collect_feishu_text_and_images(value, texts, image_keys),
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_feishu_text_and_images(value, texts, image_keys);
            }
        }
        _ => {}
    }
}

fn telegram_images(message: &Value) -> Vec<IncomingImage> {
    let Some(photo) = message.get("photo").and_then(Value::as_array) else {
        return Vec::new();
    };
    photo
        .iter()
        .max_by_key(|item| item.get("file_size").and_then(Value::as_i64).unwrap_or(0))
        .and_then(|item| item.get("file_id").and_then(Value::as_str))
        .map(|file_id| {
            vec![IncomingImage {
                source: IncomingImageSource::TelegramFile {
                    file_id: file_id.to_owned(),
                },
                filename_hint: Some("telegram-photo.jpg".to_owned()),
            }]
        })
        .unwrap_or_default()
}

fn discord_images(message: &Value) -> Vec<IncomingImage> {
    url_attachment_images(message)
}

fn qq_images(message: &Value) -> Vec<IncomingImage> {
    url_attachment_images(message)
}

fn url_attachment_images(message: &Value) -> Vec<IncomingImage> {
    message
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|attachment| {
            let url = attachment.get("url").and_then(Value::as_str)?;
            let content_type = attachment
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let filename = attachment
                .get("filename")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if !content_type.is_empty() && !content_type.starts_with("image/") {
                return None;
            }
            Some(IncomingImage {
                source: IncomingImageSource::Url {
                    url: url.to_owned(),
                },
                filename_hint: filename,
            })
        })
        .collect()
}

fn wecom_images_from_xml(xml: &str, msg_type: &str) -> Vec<IncomingImage> {
    if msg_type != "image" {
        return Vec::new();
    }
    xml_tag(xml, "MediaId")
        .map(|media_id| {
            vec![IncomingImage {
                source: IncomingImageSource::WecomMedia { media_id },
                filename_hint: Some("wecom-image.jpg".to_owned()),
            }]
        })
        .unwrap_or_default()
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

fn codex_failure_detail(
    status: Option<&std::process::ExitStatus>,
    stderr_tail: &VecDeque<String>,
    final_summary: &str,
) -> String {
    let exit = match status.and_then(std::process::ExitStatus::code) {
        Some(code) => format!("退出码 {code}"),
        None if status.is_some() => "进程被信号终止或未返回退出码".to_owned(),
        None => "进程退出状态未读取".to_owned(),
    };
    let stderr = stderr_tail
        .iter()
        .map(String::as_str)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut detail = format!("Codex 进程返回失败状态（{exit}）。");
    if !stderr.trim().is_empty() {
        detail.push_str("\nstderr 尾部：\n");
        detail.push_str(&truncate_chars(&stderr, 1600));
    }
    let summary = final_summary.trim();
    if !summary.is_empty() && summary != "任务未成功完成。" {
        detail.push_str("\n最后输出：\n");
        detail.push_str(&truncate_chars(summary, 800));
    }
    detail
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
    use super::{
        build_codex_instruction, codex_command_args, codex_failure_detail,
        collaboration_error_message, discord_images, discord_session_components,
        extract_feishu_message_parts, format_session_status, incoming_message_intent,
        incoming_text_intent, parse_codex_command, parse_session_action_data, qq_images,
        required_secret_keys, session_action_from_value, session_card, telegram_images,
        telegram_session_reply_markup, timestamp_ms, validate_gateway_model, wecom_images_from_xml,
        write_context_memory_config, write_gateway_config_to_home,
        write_gateway_session_files_to_home, CodexCommand, CollaborationManager, GoalAction,
        IncomingChatKind, IncomingImage, IncomingImageSource, IncomingMessage, IncomingTextIntent,
        NaturalRunResolution, SessionLookupScope, COLLABORATION_MODEL_CATALOG_FILENAME,
    };
    use crate::{
        database::{Repository, StoredCodexSession, StoredCollaborationBot, StoredProfile},
        domain::{
            CodexAuthMode, CodexSessionSummary, CollaborationProjectBinding, CollaborationProvider,
            MaskedCollaborationBot, MaskedProfile, ProfileKind, UpsertCollaborationBotInput,
            UpsertCollaborationProjectBindingInput,
        },
        error::AppError,
        gateway::GatewayManager,
        oauth_credentials::OAuthCredentialStore,
        secrets::{MemorySecretStore, SecretStore},
    };
    use std::{
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use uuid::Uuid;

    #[test]
    fn gateway_execution_writes_session_scoped_codex_config() {
        let root = std::env::temp_dir().join(format!(
            "codex-relay-collab-gateway-config-{}",
            Uuid::new_v4()
        ));
        write_gateway_config_to_home(
            &root,
            "https://127.0.0.1:53765",
            "third-party-coder",
            "client-key:test",
            Path::new("/tmp/codex-relay-data"),
        )
        .unwrap();
        let config = fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(config.contains(r#"model = "third-party-coder""#));
        assert!(config.contains(r#"base_url = "https://127.0.0.1:53765/v1""#));
        assert!(config.contains("--relay-gateway-token"));
        let catalog = fs::read_to_string(root.join(COLLABORATION_MODEL_CATALOG_FILENAME)).unwrap();
        assert!(catalog.contains("third-party-coder"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gateway_execution_writes_oauth_auth_and_session_scoped_config() {
        let root = std::env::temp_dir().join(format!(
            "codex-relay-collab-gateway-session-{}",
            Uuid::new_v4()
        ));
        let auth_json = r#"{"OPENAI_API_KEY":null,"tokens":{"access_token":"at-test"}}"#;

        write_gateway_session_files_to_home(
            &root,
            Some(auth_json),
            "https://127.0.0.1:53765",
            "third-party-coder",
            "client-key:test",
            Path::new("/tmp/codex-relay-data"),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("auth.json")).unwrap(),
            auth_json
        );
        let config = fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(config.contains(r#"model_provider = "codex_relay""#));
        assert!(config.contains(r#"model = "third-party-coder""#));
        assert!(config.contains(r#"base_url = "https://127.0.0.1:53765/v1""#));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gateway_model_validation_uses_backend_candidate_rules() {
        let repository = Arc::new(Repository::memory());
        insert_gateway_model_profile(&repository, "api-healthy", "third-party-coder", true);
        insert_gateway_model_profile(&repository, "api-unhealthy", "hidden-coder", false);

        validate_gateway_model(&repository, "third-party-coder").unwrap();
        let error = validate_gateway_model(&repository, "hidden-coder").unwrap_err();
        assert!(matches!(error, AppError::GatewayModelUnavailable));
    }

    #[test]
    fn gateway_binding_accepts_pool_model_without_project_oauth_profile() {
        let repository = Arc::new(Repository::memory());
        insert_profile(&repository);
        insert_gateway_model_profile(&repository, "api-healthy", "third-party-coder", true);
        insert_bot(&repository);
        let manager = manager(repository.clone());

        let binding = manager
            .upsert_binding(UpsertCollaborationProjectBindingInput {
                id: Some("binding-gateway".into()),
                bot_id: "bot-1".into(),
                project_name: "Relay".into(),
                project_slug: "relay".into(),
                working_directory: std::env::temp_dir().display().to_string(),
                profile_id: None,
                enabled: true,
                concurrency_limit: 2,
                execution_target: Some("gateway".into()),
                model_id: Some("third-party-coder".into()),
                confirmed: true,
            })
            .unwrap();

        assert_eq!(binding.execution_target, "gateway");
        assert_eq!(binding.profile_id, None);
        assert_eq!(binding.model_id.as_deref(), Some("third-party-coder"));
    }

    #[test]
    fn profile_binding_still_requires_a_runtime_profile() {
        let repository = Arc::new(Repository::memory());
        insert_gateway_model_profile(&repository, "api-healthy", "third-party-coder", true);
        insert_bot(&repository);
        let manager = manager(repository);

        let error = manager
            .upsert_binding(UpsertCollaborationProjectBindingInput {
                id: Some("binding-gateway".into()),
                bot_id: "bot-1".into(),
                project_name: "Relay".into(),
                project_slug: "relay".into(),
                working_directory: std::env::temp_dir().display().to_string(),
                profile_id: Some("api-healthy".into()),
                enabled: true,
                concurrency_limit: 2,
                execution_target: Some("profile".into()),
                model_id: None,
                confirmed: true,
            })
            .unwrap_err();

        assert!(matches!(error, AppError::ProfileRuntimeUnavailable));
    }

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
    #[test]
    fn parses_context_command_mapping() {
        assert_eq!(
            parse_codex_command("/plan 拆解发布检查").unwrap(),
            CodexCommand::Plan {
                instruction: "拆解发布检查".into()
            }
        );
        assert_eq!(
            parse_codex_command("/codex goal 完成 beta 发布").unwrap(),
            CodexCommand::Goal {
                action: GoalAction::Set("完成 beta 发布".into())
            }
        );
        assert_eq!(
            parse_codex_command(r#"/codex command:"memories off""#).unwrap(),
            CodexCommand::Memories {
                action: Some("off".into())
            }
        );
        assert_eq!(
            parse_codex_command("/codex status").unwrap(),
            CodexCommand::Status { session_id: None }
        );
        assert_eq!(
            parse_codex_command("/codex permissions danger-full-access").unwrap(),
            CodexCommand::Permissions {
                policy: Some("danger-full-access".into())
            }
        );
    }

    #[test]
    fn context_memory_config_writes_codex_toml_flags() {
        let root =
            std::env::temp_dir().join(format!("codex-relay-context-memory-{}", Uuid::new_v4()));
        write_context_memory_config(&root, true).unwrap();
        let config = fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(config.contains(r#"memories = true"#));
        assert!(config.contains(r#"use_memories = true"#));
        assert!(config.contains(r#"generate_memories = true"#));
        write_context_memory_config(&root, false).unwrap();
        let config = fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(config.contains(r#"use_memories = false"#));
        assert!(config.contains(r#"generate_memories = false"#));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parses_private_natural_text_and_group_mentions() {
        assert_eq!(
            incoming_text_intent("修复当前失败测试", IncomingChatKind::Direct),
            Some(IncomingTextIntent::NaturalTask {
                instruction: "修复当前失败测试".into(),
                force_new: false,
            })
        );
        assert_eq!(
            incoming_text_intent("修复当前失败测试", IncomingChatKind::Group),
            None
        );
        assert_eq!(
            incoming_text_intent("<@123> 新任务：修复当前失败测试", IncomingChatKind::Group),
            Some(IncomingTextIntent::NaturalTask {
                instruction: "修复当前失败测试".into(),
                force_new: true,
            })
        );
        assert_eq!(
            incoming_text_intent("@Codex /codex projects", IncomingChatKind::Group),
            Some(IncomingTextIntent::Command("/codex projects".into()))
        );
        assert_eq!(
            incoming_text_intent("@Codex 看看现在绑定的什么项目", IncomingChatKind::Group),
            Some(IncomingTextIntent::Command("/codex projects".into()))
        );
        assert_eq!(
            incoming_text_intent("看看现在绑定的什么项目", IncomingChatKind::Direct),
            Some(IncomingTextIntent::Command("/codex projects".into()))
        );
        assert_eq!(
            incoming_text_intent("看看现在绑定的什么项目", IncomingChatKind::Group),
            None
        );
        assert_eq!(
            incoming_text_intent(
                "@Codex 新任务：看看现在绑定的什么项目",
                IncomingChatKind::Group
            ),
            Some(IncomingTextIntent::NaturalTask {
                instruction: "看看现在绑定的什么项目".into(),
                force_new: true,
            })
        );
    }

    #[test]
    fn image_only_messages_become_natural_tasks_when_addressed() {
        let image = IncomingImage {
            source: IncomingImageSource::Url {
                url: "https://example.test/image.png".into(),
            },
            filename_hint: Some("image.png".into()),
        };
        let direct = IncomingMessage {
            provider: CollaborationProvider::Telegram,
            bot_id: "bot-1".into(),
            chat_id: "chat-1".into(),
            sender: Some("sender-1".into()),
            text: String::new(),
            chat_kind: IncomingChatKind::Direct,
            addressed: false,
            images: vec![image.clone()],
        };
        assert_eq!(
            incoming_message_intent(&direct),
            Some(IncomingTextIntent::NaturalTask {
                instruction: "请分析这张图片。".into(),
                force_new: false,
            })
        );

        let ignored_group = IncomingMessage {
            chat_kind: IncomingChatKind::Group,
            ..direct.clone()
        };
        assert_eq!(incoming_message_intent(&ignored_group), None);

        let addressed_group = IncomingMessage {
            chat_kind: IncomingChatKind::Group,
            addressed: true,
            ..direct
        };
        assert_eq!(
            incoming_message_intent(&addressed_group),
            Some(IncomingTextIntent::NaturalTask {
                instruction: "请分析这张图片。".into(),
                force_new: false,
            })
        );
    }

    #[test]
    fn parses_platform_image_attachments() {
        let feishu_message = serde_json::json!({
            "content": serde_json::json!({
                "text": "理解一下这张图",
                "image_key": "img_v2_abc"
            }).to_string(),
        });
        let (feishu_text, feishu_images) =
            extract_feishu_message_parts(&feishu_message, "om_message");
        assert_eq!(feishu_text, "理解一下这张图");
        assert_eq!(feishu_images.len(), 1);
        assert_eq!(
            feishu_images[0].source,
            IncomingImageSource::FeishuMessageResource {
                message_id: "om_message".into(),
                file_key: "img_v2_abc".into(),
            }
        );

        let telegram_message = serde_json::json!({
            "photo": [
                {"file_id": "small", "file_size": 10},
                {"file_id": "large", "file_size": 100}
            ]
        });
        assert_eq!(
            telegram_images(&telegram_message)[0].source,
            IncomingImageSource::TelegramFile {
                file_id: "large".into()
            }
        );

        let discord_message = serde_json::json!({
            "attachments": [
                {"url": "https://cdn.example.test/a.png", "content_type": "image/png", "filename": "a.png"},
                {"url": "https://cdn.example.test/a.txt", "content_type": "text/plain", "filename": "a.txt"}
            ]
        });
        assert_eq!(discord_images(&discord_message).len(), 1);

        let qq_message = serde_json::json!({
            "attachments": [
                {"url": "https://cdn.example.test/qq-image.jpg", "filename": "qq-image.jpg"}
            ]
        });
        assert_eq!(
            qq_images(&qq_message)[0].source,
            IncomingImageSource::Url {
                url: "https://cdn.example.test/qq-image.jpg".into()
            }
        );

        let wecom_images = wecom_images_from_xml(
            "<xml><MsgType><![CDATA[image]]></MsgType><MediaId><![CDATA[MEDIA123]]></MediaId></xml>",
            "image",
        );
        assert_eq!(
            wecom_images[0].source,
            IncomingImageSource::WecomMedia {
                media_id: "MEDIA123".into()
            }
        );
    }

    #[test]
    fn codex_exec_args_include_images_for_new_and_resume_sessions() {
        let output = PathBuf::from("/tmp/last-message.txt");
        let images = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.jpg")];

        assert_eq!(
            codex_command_args(false, None, &output, &images, None, "workspace-write").unwrap(),
            vec![
                "exec",
                "--json",
                "--output-last-message",
                "/tmp/last-message.txt",
                "--sandbox",
                "workspace-write",
                "--image",
                "/tmp/a.png",
                "--image",
                "/tmp/b.jpg",
                "-",
            ]
        );
        assert_eq!(
            codex_command_args(
                true,
                Some("codex-session-1"),
                &output,
                &images,
                Some("gpt-5.1-codex"),
                "danger-full-access",
            )
            .unwrap(),
            vec![
                "exec",
                "resume",
                "--json",
                "--output-last-message",
                "/tmp/last-message.txt",
                "--model",
                "gpt-5.1-codex",
                "-c",
                "sandbox_mode=\"danger-full-access\"",
                "--image",
                "/tmp/a.png",
                "--image",
                "/tmp/b.jpg",
                "codex-session-1",
                "-",
            ]
        );
    }

    #[test]
    fn bot_prompt_and_image_notice_are_prepended_to_codex_instruction() {
        let prompt = build_codex_instruction(Some("请用审查员口吻回复。"), "修复失败测试", 2);

        assert!(prompt.contains("机器人专属提示词：\n请用审查员口吻回复。"));
        assert!(prompt.contains("用户任务：\n修复失败测试"));
        assert!(prompt.contains("图片附件：本次消息已附加 2 张图片"));
        assert_eq!(build_codex_instruction(None, "只处理文本", 0), "只处理文本");
    }

    #[test]
    fn failure_detail_status_and_button_payloads_include_session_context() {
        let mut stderr_tail = VecDeque::new();
        stderr_tail.push_back("error: first failure".to_owned());
        stderr_tail.push_back("error: second failure".to_owned());
        let detail = codex_failure_detail(None, &stderr_tail, "最后一条模型输出");
        assert!(detail.contains("进程退出状态未读取"));
        assert!(detail.contains("stderr 尾部"));
        assert!(detail.contains("error: second failure"));
        assert!(detail.contains("最后输出"));

        let repository = Repository::memory();
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        insert_session(
            &repository,
            "button-session",
            "binding-1",
            Some("sender-1"),
            timestamp_ms(),
            "failed",
            None,
        );
        repository
            .set_codex_session_status(
                "button-session",
                "failed",
                Some("摘要"),
                Some(&detail),
                Some(timestamp_ms()),
            )
            .unwrap();
        let session = repository.codex_session("button-session").unwrap().session;
        assert!(format_session_status(&session).contains("失败原因："));
        assert_eq!(
            session_action_from_value(&serde_json::json!({
                "op": "refresh",
                "session_id": "button-session",
            })),
            Some(("refresh", "button-session"))
        );
        assert_eq!(
            parse_session_action_data("codex:cancel:button-session"),
            Some(("cancel", "button-session"))
        );
        assert!(telegram_session_reply_markup(&session)
            .to_string()
            .contains("codex:refresh:button-session"));
        assert!(!telegram_session_reply_markup(&session)
            .to_string()
            .contains("codex:cancel:button-session"));
        assert!(discord_session_components(&session)
            .to_string()
            .contains("codex:refresh:button-session"));
        assert!(session_card(&session).to_string().contains("失败原因"));
    }

    #[test]
    fn natural_tasks_auto_select_single_binding_and_prompt_for_multiple_bindings() {
        let repository = Arc::new(Repository::memory());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        let manager = manager(repository.clone());

        match manager
            .resolve_natural_run(
                CollaborationProvider::Feishu,
                "bot-1",
                "chat-1",
                "修复测试".into(),
                false,
            )
            .unwrap()
        {
            NaturalRunResolution::Ready {
                binding,
                instruction,
                force_new,
            } => {
                assert_eq!(binding.id, "binding-1");
                assert_eq!(instruction, "修复测试");
                assert!(!force_new);
            }
            NaturalRunResolution::Message(message) => {
                panic!("single binding should resolve, got {message}");
            }
        }

        insert_binding(&repository, "binding-2", "Relay Docs", "relay-docs");
        match manager
            .resolve_natural_run(
                CollaborationProvider::Feishu,
                "bot-1",
                "chat-1",
                "继续整理".into(),
                false,
            )
            .unwrap()
        {
            NaturalRunResolution::Message(message) => {
                assert!(message.contains("多个项目"));
                assert!(message.contains("Relay (relay)"));
                assert!(message.contains("/codex run <project>"));
            }
            NaturalRunResolution::Ready { .. } => {
                panic!("multiple bindings should request a project name");
            }
        }
    }

    #[test]
    fn natural_tasks_ignore_broken_resume_history_and_start_new_session() {
        let db_path = std::env::temp_dir().join(format!(
            "codex-relay-natural-resume-{}.sqlite",
            Uuid::new_v4()
        ));
        let repository = Arc::new(Repository::open(&db_path).unwrap());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute("DROP TABLE codex_sessions", [])
            .unwrap();
        let manager = manager(repository);

        match manager
            .resolve_natural_run(
                CollaborationProvider::Feishu,
                "bot-1",
                "chat-1",
                "概述一下项目".into(),
                false,
            )
            .unwrap()
        {
            NaturalRunResolution::Ready {
                binding,
                instruction,
                force_new,
            } => {
                assert_eq!(binding.id, "binding-1");
                assert_eq!(instruction, "概述一下项目");
                assert!(!force_new);
            }
            NaturalRunResolution::Message(message) => {
                panic!("broken resume history should not block natural task, got {message}");
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn natural_tasks_reuse_chat_context_and_force_new_marks_next_turn() {
        let repository = Arc::new(Repository::memory());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        let manager = manager(repository.clone());
        let binding = repository
            .collaboration_project_binding("binding-1")
            .unwrap();
        let context = manager.ensure_context_for_binding(&binding).unwrap();
        repository
            .set_collaboration_chat_context(
                CollaborationProvider::Feishu,
                "bot-1",
                "chat-1",
                &context.id,
            )
            .unwrap();

        match manager
            .resolve_natural_run(
                CollaborationProvider::Feishu,
                "bot-1",
                "chat-1",
                "继续修复".into(),
                false,
            )
            .unwrap()
        {
            NaturalRunResolution::Ready {
                binding, force_new, ..
            } => {
                assert_eq!(binding.id, "binding-1");
                assert!(!force_new);
            }
            NaturalRunResolution::Message(message) => {
                panic!("chat context should resolve binding, got {message}");
            }
        }

        match manager
            .resolve_natural_run(
                CollaborationProvider::Feishu,
                "bot-1",
                "chat-1",
                "重新做".into(),
                true,
            )
            .unwrap()
        {
            NaturalRunResolution::Ready {
                binding, force_new, ..
            } => {
                assert_eq!(binding.id, "binding-1");
                assert!(force_new);
            }
            NaturalRunResolution::Message(message) => {
                panic!("force-new task should still resolve binding, got {message}");
            }
        }
    }

    #[test]
    fn collaboration_error_messages_include_actionable_codes() {
        let profile_message = collaboration_error_message(&AppError::ProfileRuntimeUnavailable);
        assert!(profile_message.contains("OAuth"));
        assert!(profile_message.contains("profile_runtime_unavailable"));

        let gateway_message = collaboration_error_message(&AppError::GatewayModelUnavailable);
        assert!(gateway_message.contains("网关模型"));
        assert!(gateway_message.contains("gateway_model_unavailable"));

        let local_state_message = collaboration_error_message(&AppError::LocalStateUnavailable);
        assert!(local_state_message.contains("本机会话状态暂不可读"));
        assert!(local_state_message.contains("local_state_unavailable"));
        assert!(!local_state_message.contains("internal"));

        let upstream_message = collaboration_error_message(&AppError::UpstreamUnavailable);
        assert!(upstream_message.contains("上游服务"));
        assert!(upstream_message.contains("upstream_unavailable"));
        assert!(!upstream_message.contains("internal"));
    }

    #[tokio::test]
    async fn scoped_short_id_cancels_current_chat_session() {
        let repository = Arc::new(Repository::memory());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        insert_session_for_chat(
            &repository,
            TestSessionSeed {
                id: "12345678-current",
                binding_id: "binding-1",
                chat_id: "chat-1",
                sender: Some("sender-1"),
                started_at_ms: timestamp_ms(),
                status: "running",
                codex_session_id: None,
            },
        );
        insert_session_for_chat(
            &repository,
            TestSessionSeed {
                id: "12345678-other",
                binding_id: "binding-1",
                chat_id: "chat-2",
                sender: Some("sender-2"),
                started_at_ms: timestamp_ms(),
                status: "running",
                codex_session_id: None,
            },
        );
        let manager = manager(repository.clone());
        let scope = SessionLookupScope::new(CollaborationProvider::Feishu, "bot-1", "chat-1");

        let session = manager
            .cancel_session_by_reference("12345678", Some(&scope))
            .await
            .unwrap();

        assert_eq!(session.id, "12345678-current");
        assert_eq!(session.relay_status, "cancelled");
        assert_eq!(
            repository
                .codex_session("12345678-current")
                .unwrap()
                .session
                .relay_status,
            "cancelled"
        );
        assert_eq!(
            repository
                .codex_session("12345678-other")
                .unwrap()
                .session
                .relay_status,
            "running"
        );
    }

    #[tokio::test]
    async fn cancel_session_ignores_unreadable_event_log_after_status_update() {
        let db_path = std::env::temp_dir().join(format!(
            "codex-relay-cancel-event-log-{}.sqlite",
            Uuid::new_v4()
        ));
        let repository = Arc::new(Repository::open(&db_path).unwrap());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        insert_session_for_chat(
            &repository,
            TestSessionSeed {
                id: "event-log-session",
                binding_id: "binding-1",
                chat_id: "chat-1",
                sender: Some("sender-1"),
                started_at_ms: timestamp_ms(),
                status: "running",
                codex_session_id: None,
            },
        );
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute("DROP TABLE codex_session_events", [])
            .unwrap();
        let manager = manager(repository.clone());

        let session = manager
            .cancel_session_by_reference("event-log-session", None)
            .await
            .unwrap();

        assert_eq!(session.relay_status, "cancelled");
        assert_eq!(
            repository
                .codex_session("event-log-session")
                .unwrap()
                .session
                .relay_status,
            "cancelled"
        );
        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn scoped_short_id_cancel_reports_unreadable_local_state() {
        let db_path = std::env::temp_dir().join(format!(
            "codex-relay-cancel-local-state-{}.sqlite",
            Uuid::new_v4()
        ));
        let repository = Arc::new(Repository::open(&db_path).unwrap());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute("DROP TABLE codex_sessions", [])
            .unwrap();
        let manager = manager(repository);
        let scope = SessionLookupScope::new(CollaborationProvider::Feishu, "bot-1", "chat-1");

        let error = manager
            .cancel_session_by_reference("12345678", Some(&scope))
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::LocalStateUnavailable));
        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn scoped_short_id_conflict_returns_conflict() {
        let repository = Arc::new(Repository::memory());
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        insert_session(
            &repository,
            "87654321-alpha",
            "binding-1",
            Some("sender-1"),
            timestamp_ms(),
            "running",
            None,
        );
        insert_session(
            &repository,
            "87654321-beta",
            "binding-1",
            Some("sender-2"),
            timestamp_ms(),
            "running",
            None,
        );
        let manager = manager(repository);
        let scope = SessionLookupScope::new(CollaborationProvider::Feishu, "bot-1", "chat-1");

        let error = manager
            .resolve_codex_session_reference("87654321", Some(&scope))
            .unwrap_err();

        assert!(matches!(error, AppError::Conflict));
    }

    #[test]
    fn feishu_session_card_hides_cancel_action_after_terminal_status() {
        let repository = Repository::memory();
        insert_profile(&repository);
        insert_bot(&repository);
        insert_binding(&repository, "binding-1", "Relay", "relay");
        insert_session(
            &repository,
            "card-running",
            "binding-1",
            Some("sender-1"),
            timestamp_ms(),
            "running",
            None,
        );
        let running = repository.codex_session("card-running").unwrap().session;
        let running_card = session_card(&running).to_string();
        assert!(running_card.contains("刷新状态"));
        assert!(running_card.contains("取消任务"));

        for status in ["completed", "failed", "cancelled"] {
            insert_session(
                &repository,
                &format!("card-{status}"),
                "binding-1",
                Some("sender-1"),
                timestamp_ms(),
                status,
                None,
            );
            let session = repository
                .codex_session(&format!("card-{status}"))
                .unwrap()
                .session;
            let card = session_card(&session).to_string();
            assert!(card.contains("刷新状态"));
            assert!(!card.contains("取消任务"));
        }
    }

    #[tokio::test]
    async fn upserts_all_provider_bots_with_masked_metadata() {
        let repository = Arc::new(Repository::memory());
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        let gateway = Arc::new(GatewayManager::new(
            repository.clone(),
            secrets.clone(),
            oauth_credentials.clone(),
            PathBuf::from("/tmp/codex-relay-collaboration-certs"),
        ));
        let manager = CollaborationManager::new(
            repository,
            secrets,
            oauth_credentials,
            gateway,
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
                system_prompt: None,
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
                system_prompt: None,
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
                system_prompt: None,
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
                system_prompt: None,
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
                system_prompt: None,
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

    #[tokio::test]
    async fn bot_system_prompt_persists_and_updates_without_secret_resubmit() {
        let repository = Arc::new(Repository::memory());
        let manager = manager(repository.clone());

        let created = manager
            .upsert_bot(UpsertCollaborationBotInput {
                id: Some("prompt-bot".into()),
                provider: CollaborationProvider::Feishu,
                name: "Prompt Bot".into(),
                enabled: false,
                confirmed: true,
                app_id: Some("cli_prompt".into()),
                app_secret: Some("original-secret".into()),
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
                system_prompt: Some("旧提示词".into()),
            })
            .await
            .unwrap();
        assert_eq!(created.system_prompt.as_deref(), Some("旧提示词"));

        let updated = manager
            .upsert_bot(UpsertCollaborationBotInput {
                id: Some("prompt-bot".into()),
                provider: CollaborationProvider::Feishu,
                name: "Prompt Bot".into(),
                enabled: false,
                confirmed: true,
                app_id: Some("cli_prompt".into()),
                app_secret: None,
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
                system_prompt: Some("新的专属提示词".into()),
            })
            .await
            .unwrap();

        assert_eq!(updated.system_prompt.as_deref(), Some("新的专属提示词"));
        assert_eq!(
            repository
                .list_collaboration_bots()
                .unwrap()
                .into_iter()
                .find(|bot| bot.bot.id == "prompt-bot")
                .and_then(|bot| bot.bot.system_prompt)
                .as_deref(),
            Some("新的专属提示词")
        );
        assert_eq!(
            manager.bot_system_prompt("prompt-bot").unwrap().as_deref(),
            Some("新的专属提示词")
        );
        let runtime = manager.runtime("prompt-bot").await.unwrap();
        assert_eq!(
            manager.secret(&runtime, "app_secret").await.unwrap(),
            "original-secret"
        );
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

    fn manager(repository: Arc<Repository>) -> CollaborationManager {
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        let gateway = Arc::new(GatewayManager::new(
            repository.clone(),
            secrets.clone(),
            oauth_credentials.clone(),
            PathBuf::from("/tmp/codex-relay-collaboration-certs"),
        ));
        CollaborationManager::new(
            repository,
            secrets,
            oauth_credentials,
            gateway,
            PathBuf::from("/tmp/codex-relay-collaboration-test"),
        )
    }

    fn insert_profile(repository: &Repository) {
        repository
            .insert_profile(&StoredProfile {
                profile: MaskedProfile {
                    id: "profile-1".into(),
                    alias: "工作账号".into(),
                    kind: ProfileKind::CodexOauth,
                    base_url: None,
                    provider: Default::default(),
                    wire_api: Default::default(),
                    enabled: true,
                    in_pool: false,
                    priority: 0,
                    weight: 1,
                    models: Vec::new(),
                    model_mappings: Vec::new(),
                    health: "healthy".into(),
                    cooldown_until_ms: None,
                    credential_configured: true,
                    auth_mode: CodexAuthMode::OAuth,
                    is_current: false,
                    account: None,
                },
                secret_ref: Some("profile:profile-1:oauth".into()),
                credential_fingerprint: None,
            })
            .unwrap();
    }

    fn insert_gateway_model_profile(repository: &Repository, id: &str, model: &str, healthy: bool) {
        repository
            .insert_profile(&StoredProfile {
                profile: MaskedProfile {
                    id: id.into(),
                    alias: format!("API {id}"),
                    kind: ProfileKind::ApiKey,
                    base_url: Some("https://gateway.example.test".into()),
                    provider: Default::default(),
                    wire_api: Default::default(),
                    enabled: true,
                    in_pool: true,
                    priority: 0,
                    weight: 1,
                    models: vec![model.into()],
                    model_mappings: Vec::new(),
                    health: if healthy { "healthy" } else { "unhealthy" }.into(),
                    cooldown_until_ms: None,
                    credential_configured: true,
                    auth_mode: CodexAuthMode::OAuth,
                    is_current: false,
                    account: None,
                },
                secret_ref: Some(format!("profile:{id}:api-key")),
                credential_fingerprint: None,
            })
            .unwrap();
    }

    fn insert_bot(repository: &Repository) {
        repository
            .upsert_collaboration_bot(&StoredCollaborationBot {
                bot: MaskedCollaborationBot {
                    id: "bot-1".into(),
                    provider: CollaborationProvider::Feishu,
                    name: "Codex Bot".into(),
                    enabled: true,
                    connection_status: "connected".into(),
                    credential_mask: "cli_••••test".into(),
                    config_summary: "App ID cli_••••test".into(),
                    callback_public_url: None,
                    system_prompt: None,
                    last_error: None,
                    updated_at_ms: timestamp_ms(),
                },
                config_json: "{}".into(),
                secret_refs_json: "{}".into(),
            })
            .unwrap();
    }

    fn insert_binding(repository: &Repository, id: &str, name: &str, slug: &str) {
        let now = timestamp_ms();
        repository
            .upsert_collaboration_project_binding(&CollaborationProjectBinding {
                id: id.into(),
                provider: CollaborationProvider::Feishu,
                bot_id: "bot-1".into(),
                bot_name: "Codex Bot".into(),
                project_name: name.into(),
                project_slug: slug.into(),
                working_directory: "/tmp".into(),
                profile_id: Some("profile-1".into()),
                profile_alias: Some("工作账号".into()),
                chat_id: Some("chat-1".into()),
                bind_code: format!("CODE-{id}"),
                enabled: true,
                concurrency_limit: 2,
                execution_target: "profile".into(),
                model_id: None,
                created_at_ms: now,
                updated_at_ms: now,
            })
            .unwrap();
    }

    fn insert_session(
        repository: &Repository,
        id: &str,
        binding_id: &str,
        sender: Option<&str>,
        started_at_ms: i64,
        status: &str,
        codex_session_id: Option<&str>,
    ) {
        insert_session_for_chat(
            repository,
            TestSessionSeed {
                id,
                binding_id,
                chat_id: "chat-1",
                sender,
                started_at_ms,
                status,
                codex_session_id,
            },
        );
    }

    struct TestSessionSeed<'a> {
        id: &'a str,
        binding_id: &'a str,
        chat_id: &'a str,
        sender: Option<&'a str>,
        started_at_ms: i64,
        status: &'a str,
        codex_session_id: Option<&'a str>,
    }

    fn insert_session_for_chat(repository: &Repository, seed: TestSessionSeed<'_>) {
        repository
            .insert_codex_session(&StoredCodexSession {
                session: CodexSessionSummary {
                    id: seed.id.into(),
                    binding_id: seed.binding_id.into(),
                    context_id: None,
                    provider: CollaborationProvider::Feishu,
                    provider_bot_id: Some("bot-1".into()),
                    provider_chat_id: Some(seed.chat_id.into()),
                    provider_message_id: None,
                    project_name: "Relay".into(),
                    project_slug: "relay".into(),
                    profile_id: Some("profile-1".into()),
                    profile_alias: Some("工作账号".into()),
                    relay_status: seed.status.into(),
                    codex_session_id: seed.codex_session_id.map(ToOwned::to_owned),
                    feishu_message_id: None,
                    feishu_chat_id: Some(seed.chat_id.into()),
                    started_by: seed.sender.map(ToOwned::to_owned),
                    started_at_ms: seed.started_at_ms,
                    updated_at_ms: seed.started_at_ms,
                    finished_at_ms: Some(seed.started_at_ms + 1_000),
                    summary: Some("done".into()),
                    last_error: None,
                    execution_target: "profile".into(),
                    model_id: None,
                    turn_kind: "run".into(),
                    conversation_mode: "default".into(),
                    goal_status: None,
                },
                working_directory: "/tmp".into(),
            })
            .unwrap();
    }
}
