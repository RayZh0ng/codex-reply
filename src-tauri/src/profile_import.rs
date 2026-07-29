use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::Mutex,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    codex_runtime::{CodexRuntime, ImportAuthVerification, ImportAuthVerificationSource},
    database::Repository,
    domain::{
        CodexAuthMode, CommitJsonProfileImportInput, DiscardJsonProfileImportInput,
        JsonProfileImportPreview, JsonProfileImportPreviewItem, JsonProfileImportResult,
        JsonProfileImportResultItem, PreviewJsonProfileImportInput,
    },
    error::{AppError, AppResult},
    profiles::{
        create_imported_profile, imported_account_summary, update_imported_profile_credential,
        CodexOAuthCredential, ImportedAuthFileCredential,
    },
    secrets::SecretStore,
};

const MAX_FILES: usize = 100;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ITEMS: usize = 1_000;
const PREVIEW_TTL_MS: i64 = 15 * 60 * 1_000;

#[derive(Default)]
pub struct JsonProfileImportStore {
    sessions: Mutex<HashMap<String, ImportSession>>,
}

#[derive(Clone)]
struct ImportSession {
    expires_at_ms: i64,
    candidates: Vec<ImportCandidate>,
}

#[derive(Clone)]
struct ImportCandidate {
    id: String,
    file_name: String,
    alias: String,
    alias_is_fallback: bool,
    source: String,
    auth_mode: CodexAuthMode,
    status: ImportStatus,
    message: String,
    email: Option<String>,
    account_id: Option<String>,
    plan_type: Option<String>,
    fingerprint: String,
    credential: ImportedAuthFileCredential,
    has_refresh_token: bool,
    existing_profile_id: Option<String>,
    existing_profile_alias: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportStatus {
    Valid,
    Invalid,
    Unverified,
}

impl ImportStatus {
    fn name(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Unverified => "unverified",
        }
    }
}

impl JsonProfileImportStore {
    pub async fn preview(
        &self,
        input: PreviewJsonProfileImportInput,
        repository: &Repository,
        runtime: &CodexRuntime,
    ) -> AppResult<JsonProfileImportPreview> {
        self.remove_expired()?;
        if input.paths.is_empty() || input.paths.len() > MAX_FILES {
            return Err(AppError::ValidationFailed);
        }
        let mut candidates = Vec::new();
        for path in input.paths {
            let path = Path::new(&path);
            let file_name = path
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .ok_or(AppError::ValidationFailed)?
                .to_owned();
            if !file_name.to_ascii_lowercase().ends_with(".json") {
                return Err(AppError::ValidationFailed);
            }
            let metadata = fs::metadata(path).map_err(|_| AppError::ValidationFailed)?;
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                return Err(AppError::ValidationFailed);
            }
            let text = fs::read_to_string(path).map_err(|_| AppError::ValidationFailed)?;
            let value: Value =
                serde_json::from_str(&text).map_err(|_| AppError::ValidationFailed)?;
            for entry in entries_from_json(value, &file_name)? {
                if candidates.len() == MAX_ITEMS {
                    return Err(AppError::ValidationFailed);
                }
                candidates.push(candidate_from_entry(entry, &file_name));
            }
        }
        if candidates.is_empty() {
            return Err(AppError::ValidationFailed);
        }

        let stored = repository.list_profiles()?;
        let mut seen_fingerprints = HashSet::new();
        for candidate in &mut candidates {
            if candidate.status != ImportStatus::Invalid {
                if !seen_fingerprints.insert(candidate.fingerprint.clone()) {
                    candidate.status = ImportStatus::Invalid;
                    candidate.message = "与同一批中的另一项凭据重复。".to_owned();
                }
                if let Some(existing) = stored.iter().find(|stored| {
                    stored.credential_fingerprint.as_deref() == Some(candidate.fingerprint.as_str())
                        || (candidate.account_id.is_some()
                            && stored
                                .profile
                                .account
                                .as_ref()
                                .and_then(|account| account.account_id.as_ref())
                                == candidate.account_id.as_ref())
                        || (candidate.account_id.is_none()
                            && candidate.email.is_some()
                            && stored
                                .profile
                                .account
                                .as_ref()
                                .and_then(|account| account.email.as_ref())
                                == candidate.email.as_ref())
                }) {
                    candidate.existing_profile_id = Some(existing.profile.id.clone());
                    candidate.existing_profile_alias = Some(existing.profile.alias.clone());
                }
            }
            if candidate.status == ImportStatus::Invalid {
                continue;
            }
            verify_candidate(candidate, runtime).await;
            if candidate.existing_profile_id.is_none() {
                if let Some(existing) = stored.iter().find(|stored| {
                    stored.credential_fingerprint.as_deref() == Some(candidate.fingerprint.as_str())
                        || (candidate.account_id.is_some()
                            && stored
                                .profile
                                .account
                                .as_ref()
                                .and_then(|account| account.account_id.as_ref())
                                == candidate.account_id.as_ref())
                        || (candidate.account_id.is_none()
                            && candidate.email.is_some()
                            && stored
                                .profile
                                .account
                                .as_ref()
                                .and_then(|account| account.email.as_ref())
                                == candidate.email.as_ref())
                }) {
                    candidate.existing_profile_id = Some(existing.profile.id.clone());
                    candidate.existing_profile_alias = Some(existing.profile.alias.clone());
                }
            }
        }
        let mut aliases = stored
            .iter()
            .map(|item| item.profile.alias.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        for candidate in &mut candidates {
            if candidate.status == ImportStatus::Invalid {
                continue;
            }
            candidate.alias = candidate
                .existing_profile_alias
                .clone()
                .unwrap_or_else(|| unique_alias(&candidate.alias, &mut aliases));
        }

        let preview_id = Uuid::new_v4().to_string();
        let expires_at_ms = now_ms() + PREVIEW_TTL_MS;
        let preview = JsonProfileImportPreview {
            preview_id: preview_id.clone(),
            expires_at_ms,
            items: candidates.iter().map(preview_item).collect(),
        };
        self.sessions
            .lock()
            .map_err(|_| AppError::Internal)?
            .insert(
                preview_id,
                ImportSession {
                    expires_at_ms,
                    candidates,
                },
            );
        Ok(preview)
    }

    pub async fn retry(
        &self,
        input: crate::domain::RetryJsonProfileImportInput,
        runtime: &CodexRuntime,
    ) -> AppResult<JsonProfileImportPreview> {
        self.remove_expired()?;
        let mut session = self
            .sessions
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(&input.preview_id)
            .ok_or(AppError::NotFound)?;
        for candidate in &mut session.candidates {
            if candidate.status == ImportStatus::Unverified {
                verify_candidate(candidate, runtime).await;
            }
        }
        let preview = JsonProfileImportPreview {
            preview_id: input.preview_id.clone(),
            expires_at_ms: session.expires_at_ms,
            items: session.candidates.iter().map(preview_item).collect(),
        };
        self.sessions
            .lock()
            .map_err(|_| AppError::Internal)?
            .insert(input.preview_id, session);
        Ok(preview)
    }

    pub async fn commit(
        &self,
        input: CommitJsonProfileImportInput,
        repository: &Repository,
        secrets: std::sync::Arc<dyn SecretStore>,
    ) -> AppResult<JsonProfileImportResult> {
        self.remove_expired()?;
        let session = self
            .sessions
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(&input.preview_id)
            .ok_or(AppError::NotFound)?;
        if input.item_ids.is_empty() {
            return Err(AppError::ValidationFailed);
        }
        let selected = input.item_ids.into_iter().collect::<HashSet<_>>();
        let mut result = JsonProfileImportResult {
            created: 0,
            updated: 0,
            skipped: 0,
            failed: 0,
            items: Vec::new(),
        };
        for candidate in session.candidates {
            if !selected.contains(&candidate.id) {
                result.skipped += 1;
                continue;
            }
            if candidate.status != ImportStatus::Valid {
                result.failed += 1;
                result.items.push(result_item(
                    &candidate,
                    "failed",
                    "该项尚未通过验证。",
                    None,
                ));
                continue;
            }
            let account = imported_account_summary(
                candidate.email.clone(),
                candidate.account_id.clone(),
                candidate.plan_type.clone(),
            );
            let credential = match candidate.existing_profile_id.as_deref() {
                Some(id) => {
                    preserve_existing_refresh_token(
                        repository,
                        secrets.clone(),
                        id,
                        &candidate.credential,
                        candidate.has_refresh_token,
                    )
                    .await
                }
                None => Ok(candidate.credential.clone()),
            };
            let credential = match credential {
                Ok(credential) => credential,
                Err(_) => {
                    result.failed += 1;
                    result.items.push(result_item(
                        &candidate,
                        "failed",
                        "无法安全读取已有凭据，未覆盖该档案。",
                        None,
                    ));
                    continue;
                }
            };
            let outcome = match candidate.existing_profile_id.as_deref() {
                Some(id) => update_imported_profile_credential(
                    repository,
                    secrets.clone(),
                    id,
                    &credential,
                    candidate.fingerprint.clone(),
                    account,
                )
                .await
                .map(|profile| ("updated", profile.id)),
                None => create_imported_profile(
                    repository,
                    secrets.clone(),
                    candidate.alias.clone(),
                    &credential,
                    candidate.fingerprint.clone(),
                    account,
                )
                .await
                .map(|profile| ("created", profile.id)),
            };
            match outcome {
                Ok(("created", profile_id)) => {
                    result.created += 1;
                    result.items.push(result_item(
                        &candidate,
                        "created",
                        "档案已创建。",
                        Some(profile_id),
                    ));
                }
                Ok((_, profile_id)) => {
                    result.updated += 1;
                    result.items.push(result_item(
                        &candidate,
                        "updated",
                        "已有档案的凭据已更新。",
                        Some(profile_id),
                    ));
                }
                Err(_) => {
                    result.failed += 1;
                    result.items.push(result_item(
                        &candidate,
                        "failed",
                        "安全存储或数据库写入未完成。",
                        None,
                    ));
                }
            }
        }
        Ok(result)
    }

    pub fn discard(&self, input: DiscardJsonProfileImportInput) -> AppResult<()> {
        self.sessions
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(&input.preview_id);
        Ok(())
    }

    fn remove_expired(&self) -> AppResult<()> {
        let now = now_ms();
        self.sessions
            .lock()
            .map_err(|_| AppError::Internal)?
            .retain(|_, session| session.expires_at_ms > now);
        Ok(())
    }
}

async fn verify_candidate(candidate: &mut ImportCandidate, runtime: &CodexRuntime) {
    candidate.status = match runtime
        .verify_import_auth_json(&candidate.id, &candidate.credential.auth_json)
        .await
    {
        Ok(verification) => {
            let source = verification.source;
            hydrate_candidate_identity(candidate, verification);
            candidate.message = match (source, candidate.existing_profile_id.is_some()) {
                (ImportAuthVerificationSource::DirectApi, true) => {
                    "app-server 拒绝但 ChatGPT 直连验证通过；导入后会更新已有档案。".to_owned()
                }
                (ImportAuthVerificationSource::DirectApi, false) => {
                    "app-server 拒绝但 ChatGPT 直连验证通过，可导入。".to_owned()
                }
                (_, true) => "已验证；导入后会更新已有档案。".to_owned(),
                (_, false) => "已验证，可导入。".to_owned(),
            };
            ImportStatus::Valid
        }
        Err(AppError::ProfileRuntimeUnavailable) => {
            candidate.message = if candidate.has_refresh_token {
                "refresh_token 刷新失败，或直连验证也被上游拒绝。".to_owned()
            } else {
                "access token 直连验证也被上游拒绝。".to_owned()
            };
            ImportStatus::Invalid
        }
        Err(AppError::RuntimeUnavailable) | Err(AppError::UpstreamUnavailable) => {
            candidate.message = if candidate.has_refresh_token {
                "refresh_token 刷新或直连验证暂时不可达；请稍后重试预检。".to_owned()
            } else {
                "app-server 未验证通过，直连验证暂时不可达；请稍后重试预检。".to_owned()
            };
            ImportStatus::Unverified
        }
        Err(_) => {
            candidate.message = "暂时无法联网验证，请稍后重试预检。".to_owned();
            ImportStatus::Unverified
        }
    };
}

fn hydrate_candidate_identity(
    candidate: &mut ImportCandidate,
    verification: ImportAuthVerification,
) {
    let parsed = serde_json::from_str::<Value>(&verification.auth_json)
        .ok()
        .and_then(|auth_json| parse_credential(&auth_json).ok());
    candidate.credential.auth_json = verification.auth_json;
    candidate.email = verification.email.or(candidate.email.take()).or_else(|| {
        parsed
            .as_ref()
            .and_then(|credential| credential.email.clone())
    });
    candidate.account_id = verification
        .account_id
        .or(candidate.account_id.take())
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|credential| credential.account_id.clone())
        });
    candidate.plan_type = verification
        .plan_type
        .or(candidate.plan_type.take())
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|credential| credential.plan_type.clone())
        });
    if let Some(parsed) = parsed {
        candidate.fingerprint = parsed.fingerprint;
    }
    if candidate.alias_is_fallback {
        if let Some(email) = candidate.email.as_deref() {
            candidate.alias = email.to_owned();
            candidate.alias_is_fallback = false;
        } else if let Some(account_id) = candidate.account_id.as_deref() {
            candidate.alias = format!("Codex {account_id}");
            candidate.alias_is_fallback = false;
        }
    }
}

async fn preserve_existing_refresh_token(
    repository: &Repository,
    secrets: std::sync::Arc<dyn SecretStore>,
    profile_id: &str,
    incoming: &ImportedAuthFileCredential,
    incoming_has_refresh_token: bool,
) -> AppResult<ImportedAuthFileCredential> {
    if incoming.auth_mode != CodexAuthMode::OAuth || incoming_has_refresh_token {
        return Ok(incoming.clone());
    }
    let stored = repository.profile(profile_id)?;
    let Some(reference) = stored.secret_ref else {
        return Ok(incoming.clone());
    };
    let saved = secrets.get(&reference).await?;
    let existing_refresh_token = serde_json::from_str::<ImportedAuthFileCredential>(&saved)
        .ok()
        .filter(|credential| credential.auth_mode == CodexAuthMode::OAuth)
        .and_then(|credential| refresh_token_from_auth_json(&credential.auth_json))
        .or_else(|| {
            serde_json::from_str::<CodexOAuthCredential>(&saved)
                .ok()
                .and_then(|credential| credential.refresh_token)
                .filter(|value| !value.trim().is_empty())
        });
    match existing_refresh_token {
        Some(refresh_token) => with_refresh_token(incoming, refresh_token),
        None if saved.is_empty() => Ok(incoming.clone()),
        None => Err(AppError::ProfileRuntimeUnavailable),
    }
}

fn refresh_token_from_auth_json(auth_json: &str) -> Option<String> {
    serde_json::from_str::<Value>(auth_json)
        .ok()?
        .get("tokens")?
        .get("refresh_token")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn with_refresh_token(
    incoming: &ImportedAuthFileCredential,
    refresh_token: String,
) -> AppResult<ImportedAuthFileCredential> {
    let mut auth_json: Value =
        serde_json::from_str(&incoming.auth_json).map_err(|_| AppError::ValidationFailed)?;
    let tokens = auth_json
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or(AppError::ValidationFailed)?;
    tokens.insert("refresh_token".to_owned(), Value::String(refresh_token));
    Ok(ImportedAuthFileCredential {
        version: incoming.version,
        auth_mode: incoming.auth_mode.clone(),
        auth_json: serde_json::to_string(&auth_json).map_err(|_| AppError::Internal)?,
    })
}

fn entries_from_json(value: Value, file_name: &str) -> AppResult<Vec<ImportEntry>> {
    if let Some(accounts) = value.get("accounts").and_then(Value::as_array) {
        return Ok(accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let supported = account
                    .get("platform")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case("openai"))
                    && account
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.eq_ignore_ascii_case("oauth"));
                ImportEntry {
                    value: if supported {
                        account.clone()
                    } else {
                        account.get("credentials").cloned().unwrap_or(Value::Null)
                    },
                    alias: account
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    source: if supported {
                        "Sub2API 导出".to_owned()
                    } else {
                        "Sub2API 导出（不支持的账号类型）".to_owned()
                    },
                    supported,
                    fallback_alias: format!("{} #{}", file_stem(file_name), index + 1),
                }
            })
            .collect());
    }
    match value {
        Value::Array(values) => Ok(values
            .into_iter()
            .enumerate()
            .map(|(index, value)| ImportEntry {
                value,
                alias: None,
                source: "JSON 数组".to_owned(),
                supported: true,
                fallback_alias: format!("{} #{}", file_stem(file_name), index + 1),
            })
            .collect()),
        value => {
            let source = if value.get("credentials").is_some() {
                "credentials 包裹 JSON".to_owned()
            } else {
                "JSON 文件".to_owned()
            };
            Ok(vec![ImportEntry {
                value,
                alias: None,
                source,
                supported: true,
                fallback_alias: file_stem(file_name).to_owned(),
            }])
        }
    }
}

struct ImportEntry {
    value: Value,
    alias: Option<String>,
    source: String,
    supported: bool,
    fallback_alias: String,
}

fn candidate_from_entry(entry: ImportEntry, file_name: &str) -> ImportCandidate {
    let id = Uuid::new_v4().to_string();
    if !entry.supported {
        return invalid_candidate(
            id,
            file_name,
            entry.alias.unwrap_or(entry.fallback_alias),
            entry.source,
            "仅支持 Sub2API 中的 OpenAI OAuth 账号。",
        );
    }
    let parsed = parse_credential(&entry.value);
    match parsed {
        Ok(parsed) => {
            let imported_alias = entry.alias.filter(|value| !value.trim().is_empty());
            let alias_is_fallback = imported_alias.is_none() && parsed.email.is_none();
            let alias = imported_alias
                .or_else(|| parsed.email.clone())
                .unwrap_or(entry.fallback_alias);
            ImportCandidate {
                id,
                file_name: file_name.to_owned(),
                alias,
                alias_is_fallback,
                source: entry.source,
                auth_mode: parsed.auth_mode.clone(),
                status: ImportStatus::Unverified,
                message: "正在等待联网验证。".to_owned(),
                email: parsed.email,
                account_id: parsed.account_id,
                plan_type: parsed.plan_type,
                fingerprint: parsed.fingerprint,
                credential: parsed.credential,
                has_refresh_token: parsed.has_refresh_token,
                existing_profile_id: None,
                existing_profile_alias: None,
            }
        }
        Err(message) => invalid_candidate(
            id,
            file_name,
            entry.alias.unwrap_or(entry.fallback_alias),
            entry.source,
            &message,
        ),
    }
}

fn invalid_candidate(
    id: String,
    file_name: &str,
    alias: String,
    source: String,
    message: &str,
) -> ImportCandidate {
    ImportCandidate {
        id,
        file_name: file_name.to_owned(),
        alias,
        alias_is_fallback: false,
        source,
        auth_mode: CodexAuthMode::OAuth,
        status: ImportStatus::Invalid,
        message: message.to_owned(),
        email: None,
        account_id: None,
        plan_type: None,
        fingerprint: String::new(),
        credential: ImportedAuthFileCredential {
            version: 1,
            auth_mode: CodexAuthMode::OAuth,
            auth_json: String::new(),
        },
        has_refresh_token: false,
        existing_profile_id: None,
        existing_profile_alias: None,
    }
}

struct ParsedCredential {
    auth_mode: CodexAuthMode,
    credential: ImportedAuthFileCredential,
    email: Option<String>,
    account_id: Option<String>,
    plan_type: Option<String>,
    fingerprint: String,
    has_refresh_token: bool,
}

fn parse_credential(value: &Value) -> Result<ParsedCredential, String> {
    let wrapper = value.as_object();
    let payload = wrapper
        .and_then(|object| object.get("credentials"))
        .unwrap_or(value);
    if let Some(token) = payload.as_str() {
        return parse_token_value(
            token,
            string_at_any(
                payload.as_object(),
                wrapper,
                &[&["email"], &["user", "email"]],
            ),
            string_at_any(
                payload.as_object(),
                wrapper,
                &[
                    &["account_id"],
                    &["accountId"],
                    &["chatgpt_account_id"],
                    &["chatgptAccountId"],
                ],
            ),
            string_at_any(
                payload.as_object(),
                wrapper,
                &[&["plan_type"], &["planType"]],
            ),
            None,
        );
    }
    let object = payload
        .as_object()
        .ok_or_else(|| "JSON 项必须是对象或 token 字符串。".to_owned())?;
    let auth_mode = string_at_any(Some(object), wrapper, &[&["auth_mode"], &["authMode"]]);
    if is_agent_identity_mode(auth_mode.as_deref())
        || object.contains_key("agent_identity")
        || object.contains_key("agentIdentity")
        || looks_like_agent_identity_object(object)
    {
        let identity = object
            .get("agent_identity")
            .or_else(|| object.get("agentIdentity"))
            .cloned()
            .or_else(|| {
                if looks_like_agent_identity_object(object) {
                    Some(Value::Object(object.clone()))
                } else {
                    None
                }
            })
            .ok_or_else(|| "Agent Identity 缺少 agent_identity/agentIdentity。".to_owned())?;
        let identity = canonical_agent_identity(identity)?;
        validate_agent_identity(&identity)?;
        let identity_object = identity.as_object();
        let account_id = string_at_any(
            identity_object,
            wrapper,
            &[
                &["account_id"],
                &["accountId"],
                &["chatgpt_account_id"],
                &["chatgptAccountId"],
            ],
        );
        let email = string_at_any(identity_object, wrapper, &[&["email"], &["user", "email"]]);
        let plan_type = string_at_any(identity_object, wrapper, &[&["plan_type"], &["planType"]]);
        let auth_json = json!({"auth_mode":"agent_identity","agent_identity":identity}).to_string();
        return Ok(ParsedCredential {
            auth_mode: CodexAuthMode::AgentIdentity,
            credential: ImportedAuthFileCredential {
                version: 1,
                auth_mode: CodexAuthMode::AgentIdentity,
                auth_json: auth_json.clone(),
            },
            email,
            account_id,
            plan_type,
            fingerprint: fingerprint(&auth_json),
            has_refresh_token: false,
        });
    }
    let pat = string_at(object, &["personal_access_token"])
        .or_else(|| string_at(object, &["personalAccessToken"]))
        .or_else(|| {
            string_at_any(
                None,
                wrapper,
                &[&["personal_access_token"], &["personalAccessToken"]],
            )
        });
    let access = string_at(object, &["tokens", "access_token"])
        .or_else(|| string_at(object, &["tokens", "accessToken"]))
        .or_else(|| string_at(object, &["credentials", "access_token"]))
        .or_else(|| string_at(object, &["credentials", "accessToken"]))
        .or_else(|| string_at(object, &["access_token"]))
        .or_else(|| string_at(object, &["accessToken"]))
        .or_else(|| string_at(object, &["session", "access_token"]))
        .or_else(|| string_at(object, &["session", "accessToken"]))
        .or_else(|| string_at_any(None, wrapper, &[&["access_token"], &["accessToken"]]));
    let refresh = string_at(object, &["tokens", "refresh_token"])
        .or_else(|| string_at(object, &["tokens", "refreshToken"]))
        .or_else(|| string_at(object, &["credentials", "refresh_token"]))
        .or_else(|| string_at(object, &["credentials", "refreshToken"]))
        .or_else(|| string_at(object, &["refresh_token"]))
        .or_else(|| string_at(object, &["refreshToken"]))
        .or_else(|| string_at(object, &["session", "refresh_token"]))
        .or_else(|| string_at(object, &["session", "refreshToken"]))
        .or_else(|| string_at_any(None, wrapper, &[&["refresh_token"], &["refreshToken"]]));
    let id_token = string_at(object, &["tokens", "id_token"])
        .or_else(|| string_at(object, &["tokens", "idToken"]))
        .or_else(|| string_at(object, &["credentials", "id_token"]))
        .or_else(|| string_at(object, &["credentials", "idToken"]))
        .or_else(|| string_at(object, &["id_token"]))
        .or_else(|| string_at(object, &["idToken"]))
        .or_else(|| string_at(object, &["session", "id_token"]))
        .or_else(|| string_at(object, &["session", "idToken"]))
        .or_else(|| string_at_any(None, wrapper, &[&["id_token"], &["idToken"]]));
    let email = string_at_any(
        Some(object),
        wrapper,
        &[&["email"], &["user", "email"], &["session", "email"]],
    );
    let account_id = string_at(object, &["tokens", "account_id"])
        .or_else(|| string_at(object, &["tokens", "accountId"]))
        .or_else(|| string_at(object, &["tokens", "chatgpt_account_id"]))
        .or_else(|| string_at(object, &["tokens", "chatgptAccountId"]))
        .or_else(|| string_at(object, &["credentials", "account_id"]))
        .or_else(|| string_at(object, &["credentials", "accountId"]))
        .or_else(|| string_at(object, &["credentials", "chatgpt_account_id"]))
        .or_else(|| string_at(object, &["credentials", "chatgptAccountId"]))
        .or_else(|| string_at(object, &["account_id"]))
        .or_else(|| string_at(object, &["accountId"]))
        .or_else(|| string_at(object, &["chatgpt_account_id"]))
        .or_else(|| string_at(object, &["chatgptAccountId"]))
        .or_else(|| string_at(object, &["session", "account_id"]))
        .or_else(|| string_at(object, &["session", "accountId"]))
        .or_else(|| string_at(object, &["session", "chatgpt_account_id"]))
        .or_else(|| string_at(object, &["session", "chatgptAccountId"]))
        .or_else(|| {
            string_at_any(
                None,
                wrapper,
                &[
                    &["account_id"],
                    &["accountId"],
                    &["chatgpt_account_id"],
                    &["chatgptAccountId"],
                ],
            )
        });
    let plan_type = string_at_any(
        Some(object),
        wrapper,
        &[
            &["plan_type"],
            &["planType"],
            &["session", "plan_type"],
            &["session", "planType"],
        ],
    );
    if let Some(pat) = pat {
        return parse_token_value(&pat, email, account_id, plan_type, Some("pat"));
    }
    parse_oauth(access, refresh, id_token, email, account_id, plan_type)
}

fn parse_token_value(
    token: &str,
    email: Option<String>,
    account_id: Option<String>,
    plan_type: Option<String>,
    source: Option<&str>,
) -> Result<ParsedCredential, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("token 不能为空。".to_owned());
    }
    if token.starts_with("at-") || source == Some("pat") {
        if !token.starts_with("at-") {
            return Err("personal_access_token 必须以 at- 开头。".to_owned());
        }
        let auth_json = json!({"personal_access_token":token}).to_string();
        return Ok(ParsedCredential {
            auth_mode: CodexAuthMode::PersonalAccessToken,
            credential: ImportedAuthFileCredential {
                version: 1,
                auth_mode: CodexAuthMode::PersonalAccessToken,
                auth_json: auth_json.clone(),
            },
            email,
            account_id,
            plan_type,
            fingerprint: fingerprint(token),
            has_refresh_token: false,
        });
    }
    parse_oauth(
        Some(token.to_owned()),
        None,
        None,
        email,
        account_id,
        plan_type,
    )
}

fn parse_oauth(
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    email: Option<String>,
    account_id: Option<String>,
    plan_type: Option<String>,
) -> Result<ParsedCredential, String> {
    let access_token = access_token.filter(|value| !value.trim().is_empty());
    let refresh_token = refresh_token.filter(|value| !value.trim().is_empty());
    if access_token.is_none() && refresh_token.is_none() {
        return Err("缺少 accessToken/access_token 或 refresh_token。".to_owned());
    }
    if let Some(token) = access_token.as_deref() {
        reject_expired_jwt(token)?;
    }
    let mut resolved_email = email;
    let mut resolved_account_id = account_id;
    for token in [id_token.as_deref(), access_token.as_deref()]
        .into_iter()
        .flatten()
    {
        let claims = jwt_claims(token);
        resolved_email = resolved_email.or_else(|| {
            claims
                .get("email")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        resolved_account_id = resolved_account_id.or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| {
                    claims
                        .get("chatgpt_account_id")
                        .or_else(|| claims.get("chatgptAccountId"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
        });
    }
    let auth_json = json!({
        "auth_mode": null,
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token.clone().unwrap_or_default(),
            "access_token": access_token.clone().unwrap_or_default(),
            "refresh_token": refresh_token.clone().unwrap_or_default(),
            "account_id": resolved_account_id,
        },
        "agent_identity": null,
        "personal_access_token": null,
    })
    .to_string();
    Ok(ParsedCredential {
        auth_mode: CodexAuthMode::OAuth,
        credential: ImportedAuthFileCredential {
            version: 1,
            auth_mode: CodexAuthMode::OAuth,
            auth_json,
        },
        email: resolved_email,
        account_id: resolved_account_id,
        plan_type,
        fingerprint: fingerprint(
            access_token
                .as_deref()
                .or(refresh_token.as_deref())
                .unwrap_or_default(),
        ),
        has_refresh_token: refresh_token.is_some(),
    })
}

fn is_agent_identity_mode(value: Option<&str>) -> bool {
    value
        .map(|value| {
            value
                .chars()
                .filter(|character| {
                    *character != '_' && *character != '-' && !character.is_whitespace()
                })
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .is_some_and(|value| value == "agentidentity")
}

fn looks_like_agent_identity_object(object: &serde_json::Map<String, Value>) -> bool {
    string_at_any(
        Some(object),
        None,
        &[
            &["agent_runtime_id"],
            &["agentRuntimeId"],
            &["agent_private_key"],
            &["agentPrivateKey"],
        ],
    )
    .is_some()
        && string_at_any(
            Some(object),
            None,
            &[
                &["account_id"],
                &["accountId"],
                &["chatgpt_account_id"],
                &["chatgptAccountId"],
            ],
        )
        .is_some()
}

fn canonical_agent_identity(value: Value) -> Result<Value, String> {
    let Value::Object(mut object) = value else {
        return Ok(value);
    };
    let agent_runtime_id = string_at_any(
        Some(&object),
        None,
        &[
            &["agent_runtime_id"],
            &["agentRuntimeId"],
            &["runtime_id"],
            &["runtimeId"],
        ],
    );
    let agent_private_key = string_at_any(
        Some(&object),
        None,
        &[
            &["agent_private_key"],
            &["agentPrivateKey"],
            &["private_key"],
            &["privateKey"],
        ],
    );
    let account_id = string_at_any(
        Some(&object),
        None,
        &[
            &["account_id"],
            &["accountId"],
            &["chatgpt_account_id"],
            &["chatgptAccountId"],
        ],
    );
    let chatgpt_user_id = string_at_any(
        Some(&object),
        None,
        &[
            &["chatgpt_user_id"],
            &["chatgptUserId"],
            &["user_id"],
            &["userId"],
        ],
    );
    for (key, value) in [
        ("agent_runtime_id", agent_runtime_id),
        ("agent_private_key", agent_private_key),
        ("account_id", account_id),
        ("chatgpt_user_id", chatgpt_user_id),
    ] {
        if let Some(value) = value {
            object
                .entry(key.to_owned())
                .or_insert_with(|| Value::String(value));
        }
    }
    Ok(Value::Object(object))
}

fn validate_agent_identity(value: &Value) -> Result<(), String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => {
            if !jwt_claims(value).is_object() {
                return Err("Agent Identity JWT 格式无效。".to_owned());
            }
            reject_expired_jwt(value)
        }
        Value::Object(value)
            if [
                "agent_runtime_id",
                "agent_private_key",
                "account_id",
                "chatgpt_user_id",
            ]
            .iter()
            .all(|key| {
                value
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
            }) =>
        {
            Ok(())
        }
        _ => Err("Agent Identity 缺少必要字段。".to_owned()),
    }
}

fn string_at_any(
    object: Option<&serde_json::Map<String, Value>>,
    wrapper: Option<&serde_json::Map<String, Value>>,
    paths: &[&[&str]],
) -> Option<String> {
    paths.iter().find_map(|path| {
        object
            .and_then(|object| string_at(object, path))
            .or_else(|| wrapper.and_then(|wrapper| string_at(wrapper, path)))
    })
}

fn string_at(object: &serde_json::Map<String, Value>, path: &[&str]) -> Option<String> {
    let mut value = object.get(*path.first()?)?;
    for key in &path[1..] {
        value = value.get(*key)?;
    }
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn reject_expired_jwt(token: &str) -> Result<(), String> {
    let claims = jwt_claims(token);
    if claims
        .get("exp")
        .and_then(Value::as_i64)
        .is_some_and(|expires| expires * 1_000 < now_ms())
    {
        Err("认证 JWT 已过期。".to_owned())
    } else {
        Ok(())
    }
}

fn jwt_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice(&payload).ok())
        .unwrap_or(Value::Null)
}

fn fingerprint(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

fn unique_alias(value: &str, used: &mut HashSet<String>) -> String {
    let base = value.trim().chars().take(120).collect::<String>();
    let base = if base.is_empty() {
        "导入的 Codex 档案".to_owned()
    } else {
        base
    };
    if used.insert(base.to_ascii_lowercase()) {
        return base;
    }
    for index in 2..=1_000 {
        let candidate = format!("{base} ({index})");
        if used.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
    }
    format!("{} {}", base, Uuid::new_v4())
}

fn file_stem(file_name: &str) -> &str {
    file_name
        .strip_suffix(".json")
        .or_else(|| file_name.strip_suffix(".JSON"))
        .unwrap_or(file_name)
}

fn preview_item(candidate: &ImportCandidate) -> JsonProfileImportPreviewItem {
    JsonProfileImportPreviewItem {
        id: candidate.id.clone(),
        file_name: candidate.file_name.clone(),
        alias: candidate.alias.clone(),
        auth_mode: candidate.auth_mode.clone(),
        source: candidate.source.clone(),
        status: candidate.status.name().to_owned(),
        message: candidate.message.clone(),
        email: candidate.email.clone(),
        account_id: candidate.account_id.clone(),
        existing_profile_alias: candidate.existing_profile_alias.clone(),
    }
}

fn result_item(
    candidate: &ImportCandidate,
    action: &str,
    message: &str,
    profile_id: Option<String>,
) -> JsonProfileImportResultItem {
    JsonProfileImportResultItem {
        id: candidate.id.clone(),
        alias: candidate.alias.clone(),
        action: action.to_owned(),
        message: message.to_owned(),
        profile_id,
        auth_mode: matches!(action, "created" | "updated").then(|| candidate.auth_mode.clone()),
    }
}

fn now_ms() -> i64 {
    crate::profiles::timestamp_ms()
}

#[cfg(test)]
mod tests {
    use super::{entries_from_json, parse_credential, with_refresh_token, ImportStatus};
    use crate::domain::CodexAuthMode;
    use serde_json::json;

    #[test]
    fn parses_canonical_oauth_and_access_only_inputs() {
        let complete = parse_credential(&json!({
            "tokens": {"id_token":"id", "access_token":"access", "refresh_token":"refresh"}
        }))
        .unwrap();
        assert_eq!(complete.auth_mode, CodexAuthMode::OAuth);

        let access_only = parse_credential(&json!({"accessToken":"opaque-access"})).unwrap();
        assert_eq!(access_only.auth_mode, CodexAuthMode::OAuth);

        let session = parse_credential(&json!({
            "session": {"accessToken":"session-access", "refreshToken":"session-refresh"}
        }))
        .unwrap();
        assert!(session.has_refresh_token);
        assert_eq!(session.auth_mode, CodexAuthMode::OAuth);
    }

    #[test]
    fn parses_credentials_wrappers_and_chatgpt_account_aliases() {
        let wrapped = parse_credential(&json!({
            "name": "Wrapped",
            "email": "wrapped@example.com",
            "chatgpt_account_id": "account-wrapped",
            "credentials": {
                "accessToken": "wrapped-access",
                "refreshToken": "wrapped-refresh"
            }
        }))
        .unwrap();
        assert_eq!(wrapped.auth_mode, CodexAuthMode::OAuth);
        assert!(wrapped.has_refresh_token);
        assert_eq!(wrapped.email.as_deref(), Some("wrapped@example.com"));
        assert_eq!(wrapped.account_id.as_deref(), Some("account-wrapped"));

        let entries = entries_from_json(
            json!({
                "accounts": [{
                    "name": "Sub2API",
                    "platform": "openai",
                    "type": "oauth",
                    "chatgptAccountId": "account-sub2api",
                    "credentials": {"access_token": "sub2api-access"}
                }]
            }),
            "sub2api.json",
        )
        .unwrap();
        let parsed = parse_credential(&entries[0].value).unwrap();
        assert_eq!(entries[0].source, "Sub2API 导出");
        assert_eq!(parsed.account_id.as_deref(), Some("account-sub2api"));
    }

    #[test]
    fn parses_pat_and_agent_identity_without_exposing_tokens() {
        let pat = parse_credential(&json!({
            "personal_access_token":"at-secret",
            "email":"pat@example.com",
            "account_id":"account-pat",
            "plan_type":"pro"
        }))
        .unwrap();
        assert_eq!(pat.auth_mode, CodexAuthMode::PersonalAccessToken);
        assert_eq!(pat.email.as_deref(), Some("pat@example.com"));
        assert_eq!(pat.account_id.as_deref(), Some("account-pat"));
        assert_eq!(pat.plan_type.as_deref(), Some("pro"));
        assert!(!format!("{:?}", pat.credential).contains("at-secret"));

        let agent = parse_credential(&json!({
            "auth_mode":"agent_identity",
            "agent_identity": {
                "agent_runtime_id":"runtime", "agent_private_key":"private", "account_id":"account", "chatgpt_user_id":"user"
            }
        }))
        .unwrap();
        assert_eq!(agent.auth_mode, CodexAuthMode::AgentIdentity);
        let agent_jwt = parse_credential(&json!({
            "agent_identity": "header.eyJhY2NvdW50X2lkIjoiYWNjb3VudCJ9.signature"
        }))
        .unwrap();
        assert_eq!(agent_jwt.auth_mode, CodexAuthMode::AgentIdentity);
        let camel_agent = parse_credential(&json!({
            "authMode":"agentIdentity",
            "credentials": {
                "agentIdentity": {
                    "agentRuntimeId":"runtime-camel",
                    "agentPrivateKey":"private-camel",
                    "chatgptAccountId":"account-camel",
                    "chatgptUserId":"user-camel",
                    "email":"camel@example.com",
                    "planType":"team"
                }
            }
        }))
        .unwrap();
        assert_eq!(camel_agent.auth_mode, CodexAuthMode::AgentIdentity);
        assert_eq!(camel_agent.email.as_deref(), Some("camel@example.com"));
        assert_eq!(camel_agent.account_id.as_deref(), Some("account-camel"));
        let auth_json: serde_json::Value =
            serde_json::from_str(&camel_agent.credential.auth_json).unwrap();
        assert_eq!(
            auth_json["agent_identity"]["agent_runtime_id"],
            serde_json::Value::String("runtime-camel".to_owned())
        );
        assert_eq!(
            auth_json["agent_identity"]["chatgpt_user_id"],
            serde_json::Value::String("user-camel".to_owned())
        );
        assert!(parse_credential(&json!({"agent_identity":"not-a-jwt"})).is_err());
        assert!(parse_credential(&json!({"session_token":"browser-cookie"})).is_err());
    }

    #[test]
    fn exposes_only_openai_oauth_entries_from_sub2api() {
        let entries = entries_from_json(
            json!({"accounts":[
                {"name":"OpenAI", "platform":"OpenAI", "type":"OAuth", "credentials":{"access_token":"a"}},
                {"name":"Other", "platform":"anthropic", "type":"oauth", "credentials":{}}
            ]}),
            "export.json",
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].supported);
        assert!(!entries[1].supported);
        assert_eq!(ImportStatus::Valid.name(), "valid");
    }

    #[test]
    fn preserves_a_saved_refresh_token_for_access_only_updates() {
        let incoming = parse_credential(&json!({"access_token":"new-access"})).unwrap();
        let merged = with_refresh_token(&incoming.credential, "saved-refresh".to_owned()).unwrap();
        let auth_json: serde_json::Value = serde_json::from_str(&merged.auth_json).unwrap();
        assert_eq!(
            auth_json["tokens"]["refresh_token"],
            serde_json::Value::String("saved-refresh".to_owned())
        );
        assert!(!format!("{merged:?}").contains("saved-refresh"));
    }
}
