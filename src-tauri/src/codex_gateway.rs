use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use toml_edit::{value, Array, DocumentMut, Item, Table};
use uuid::Uuid;

use crate::{
    codex_environment::default_codex_home,
    database::{Repository, StoredProfile},
    domain::{
        CodexAuthMode, GatewayCodexConfigStatus, GatewayModelMapping, GatewayOAuthProfileOption,
        GatewayProvider, GatewayStatus, GatewayWireApi, MaskedClientKey, ProfileKind,
        SetCodexGatewayOAuthProfileInput, GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
        GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
    },
    error::{AppError, AppResult},
    oauth_credentials::{CredentialAccess, OAuthCredentialStore},
    profiles::timestamp_ms,
    secrets::SecretStore,
};

const SNAPSHOT_REF: &str = "gateway:codex-config-snapshot";
const CODEX_CLIENT_KEY_NAME: &str = "Codex CLI Gateway";
const CODEX_RELAY_MODEL_CATALOG_FILENAME: &str = "codex-relay-model-catalog.json";
const CODEX_RELAY_GATEWAY_PROVIDER: &str = "codex_relay";
const CODEX_RELAY_DIRECT_PROVIDER: &str = "codex_relay_direct";

#[derive(Serialize, Deserialize)]
struct ConfigSnapshot {
    original: String,
    generated_hash: String,
}

pub async fn status(repository: &Repository) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    status_for_path(repository, &path)
}

fn status_for_path(repository: &Repository, path: &Path) -> AppResult<GatewayCodexConfigStatus> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let document = content.parse::<DocumentMut>().ok();
    let provider = document
        .as_ref()
        .and_then(|document| document.get("model_provider")?.as_str().map(str::to_owned));
    let mode = match provider.as_deref() {
        Some(CODEX_RELAY_GATEWAY_PROVIDER) => "relay_gateway",
        Some(CODEX_RELAY_DIRECT_PROVIDER) => "third_party",
        _ => "official",
    };
    let enabled = mode != "official";
    let auth_config = document
        .as_ref()
        .and_then(|document| relay_auth_config(document, provider.as_deref()?));
    let service_url = document.as_ref().and_then(|document| {
        document
            .get("model_providers")?
            .get(provider.as_deref()?)?
            .get("base_url")?
            .as_str()
            .map(str::to_owned)
    });
    let auth_token_readable = auth_config.as_ref().is_some_and(|config| {
        crate::read_relay_gateway_token(&config.secret_ref, &config.data_dir).is_ok()
    });
    let auth_status = if enabled && auth_config.is_none() {
        "legacy"
    } else if enabled && !auth_token_readable {
        "invalid"
    } else if enabled {
        "ok"
    } else {
        "missing"
    };
    let oauth_profile = codex_oauth_profile_status(repository)?;
    let oauth_profile_options = codex_oauth_profile_options(repository)?;
    let direct_profile = direct_profile_status(repository)?;
    Ok(GatewayCodexConfigStatus {
        enabled,
        mode: mode.to_owned(),
        config_path: path.display().to_string(),
        service_url,
        auth_status: auth_status.to_owned(),
        needs_repair: matches!(auth_status, "legacy" | "invalid"),
        message: status_message(
            mode,
            auth_config.is_some(),
            auth_token_readable,
            &direct_profile,
        ),
        direct_profile_id: direct_profile.id,
        direct_profile_alias: direct_profile.alias,
        oauth_profile_id: oauth_profile.id,
        oauth_profile_alias: oauth_profile.alias,
        oauth_profile_available: oauth_profile.available,
        oauth_profile_options,
        history_sync: None,
        history_sync_status: None,
    })
}

#[derive(Debug, Clone)]
struct DirectProfileStatus {
    id: Option<String>,
    alias: Option<String>,
}

fn direct_profile_status(repository: &Repository) -> AppResult<DirectProfileStatus> {
    let Some(profile_id) = repository.setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)? else {
        return Ok(DirectProfileStatus {
            id: None,
            alias: None,
        });
    };
    let alias = repository
        .profile(&profile_id)
        .ok()
        .map(|stored| stored.profile.alias);
    Ok(DirectProfileStatus {
        id: Some(profile_id),
        alias,
    })
}

fn status_message(
    mode: &str,
    has_auth_config: bool,
    auth_token_readable: bool,
    direct_profile: &DirectProfileStatus,
) -> String {
    if mode == "official" {
        return "Codex 正在使用官方模型配置。".to_owned();
    }
    if !has_auth_config {
        return "Codex Relay 托管配置来自旧凭据存储；请重新应用配置以使用本地凭据库。".to_owned();
    }
    if !auth_token_readable {
        return match mode {
            "third_party" => {
                "第三方供应商 API Key 引用已失效；请重新切换该供应商以修复 Codex 鉴权。"
                    .to_owned()
            }
            _ => {
                "Codex Relay 网关 Client Key 已失效；请重新启用网关配置以自动修复 Codex Client Key。"
                    .to_owned()
            }
        };
    }
    match mode {
        "third_party" => direct_profile
            .alias
            .as_deref()
            .map(|alias| format!("Codex 正在直连第三方模型提供商：{alias}。"))
            .unwrap_or_else(|| "Codex 正在直连第三方模型提供商。".to_owned()),
        _ => "Codex 正在使用 Relay 网关配置。".to_owned(),
    }
}

pub async fn enable(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    enable_for_path(
        repository,
        secrets,
        oauth_credentials,
        gateway,
        data_dir,
        &path,
    )
    .await
}

async fn enable_for_path(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
    path: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    if !gateway.running || !gateway.certificate_ready {
        return Err(AppError::GatewayNotRunning);
    }
    if gateway.available_profiles == 0 {
        return Err(AppError::UpstreamUnavailable);
    }
    let model_mappings = gateway_model_mappings(repository)?;
    let auth_json = codex_oauth_profile_auth_json(
        repository,
        &oauth_credentials,
        CredentialAccess::UserInitiated,
    )
    .await?;
    let secret_ref = ensure_codex_client_key(repository, secrets.clone()).await?;
    let mut status = enable_at_path(
        secrets,
        gateway,
        data_dir,
        path,
        &secret_ref,
        &model_mappings,
    )
    .await?;
    repository.delete_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)?;
    if let Some(auth_json) = auth_json.as_deref() {
        project_auth_json_to_default_codex_home(auth_json)?;
        status.message = format!("{} OAuth 登录档案已同步到默认 Codex 凭据。", status.message);
    }
    let oauth = codex_oauth_profile_status(repository)?;
    status.oauth_profile_id = oauth.id;
    status.oauth_profile_alias = oauth.alias;
    status.oauth_profile_available = oauth.available;
    status.oauth_profile_options = codex_oauth_profile_options(repository)?;
    Ok(status)
}

async fn enable_at_path(
    secrets: Arc<dyn SecretStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
    path: &Path,
    secret_ref: &str,
    model_mappings: &[GatewayModelMapping],
) -> AppResult<GatewayCodexConfigStatus> {
    let original = fs::read_to_string(path).unwrap_or_default();
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    document["model_provider"] = value(CODEX_RELAY_GATEWAY_PROVIDER);
    if let Some(model) = model_mappings.first() {
        document["model"] = value(model.model.clone());
    }
    document["model_catalog_json"] = value(CODEX_RELAY_MODEL_CATALOG_FILENAME);
    let providers = document["model_providers"].or_insert(Item::Table(Table::new()));
    let providers = providers
        .as_table_like_mut()
        .ok_or(AppError::ValidationFailed)?;
    let mut provider = Table::new();
    provider["name"] = value("Codex Relay LAN Gateway");
    provider["base_url"] = value(format!("{}/v1", gateway.service_url));
    provider["wire_api"] = value("responses");
    let mut auth = Table::new();
    auth["command"] = value(
        std::env::current_exe()
            .map_err(|_| AppError::RuntimeUnavailable)?
            .display()
            .to_string(),
    );
    let mut args = Array::new();
    push_relay_auth_args(&mut args, secret_ref, data_dir);
    auth["args"] = Item::Value(args.into());
    provider["auth"] = Item::Table(auth);
    providers.insert(CODEX_RELAY_GATEWAY_PROVIDER, Item::Table(provider));
    let generated = document.to_string();
    store_config_snapshot(secrets, &original, &generated).await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
        write_model_catalog(
            &parent.join(CODEX_RELAY_MODEL_CATALOG_FILENAME),
            model_mappings,
        )?;
    }
    fs::write(path, generated).map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(GatewayCodexConfigStatus {
        enabled: true,
        mode: "relay_gateway".to_owned(),
        config_path: path.display().to_string(),
        service_url: Some(gateway.service_url),
        auth_status: "ok".to_owned(),
        needs_repair: false,
        message: "Codex 已切换到 Relay 网关。请在系统中信任 Relay 导出的 CA 后启动新会话。"
            .to_owned(),
        direct_profile_id: None,
        direct_profile_alias: None,
        oauth_profile_id: None,
        oauth_profile_alias: None,
        oauth_profile_available: false,
        oauth_profile_options: Vec::new(),
        history_sync: None,
        history_sync_status: None,
    })
}

async fn store_config_snapshot(
    secrets: Arc<dyn SecretStore>,
    current: &str,
    generated: &str,
) -> AppResult<()> {
    let original = match secrets.get(SNAPSHOT_REF).await {
        Ok(value) => serde_json::from_str::<ConfigSnapshot>(&value)
            .ok()
            .filter(|snapshot| content_hash(current) == snapshot.generated_hash)
            .map(|snapshot| snapshot.original)
            .unwrap_or_else(|| current.to_owned()),
        Err(_) => current.to_owned(),
    };
    let snapshot = ConfigSnapshot {
        original,
        generated_hash: content_hash(generated),
    };
    secrets
        .set(
            SNAPSHOT_REF,
            &serde_json::to_string(&snapshot).map_err(|_| AppError::Internal)?,
        )
        .await
}

#[derive(Debug, Clone)]
struct CodexOAuthProfileStatus {
    id: Option<String>,
    alias: Option<String>,
    available: bool,
}

fn codex_oauth_profile_status(repository: &Repository) -> AppResult<CodexOAuthProfileStatus> {
    codex_oauth_profile_status_for_id(
        repository,
        repository.setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)?,
    )
}

fn codex_oauth_profile_status_for_id(
    repository: &Repository,
    profile_id: Option<String>,
) -> AppResult<CodexOAuthProfileStatus> {
    let Some(profile_id) = profile_id else {
        return Ok(CodexOAuthProfileStatus {
            id: None,
            alias: None,
            available: false,
        });
    };
    let stored = repository.profile(&profile_id).ok();
    let alias = stored.as_ref().map(|stored| stored.profile.alias.clone());
    let available = stored.as_ref().is_some_and(is_codex_oauth_unlock_profile);
    Ok(CodexOAuthProfileStatus {
        id: Some(profile_id),
        alias,
        available,
    })
}

fn codex_oauth_profile_options(
    repository: &Repository,
) -> AppResult<Vec<GatewayOAuthProfileOption>> {
    let mut options = repository
        .list_profiles()?
        .into_iter()
        .filter_map(|stored| {
            let profile = &stored.profile;
            (profile.kind == ProfileKind::CodexOauth && profile.auth_mode == CodexAuthMode::OAuth)
                .then(|| GatewayOAuthProfileOption {
                    reason: oauth_profile_unavailable_reason(&stored),
                    available: is_codex_oauth_unlock_profile(&stored),
                    id: profile.id.clone(),
                    alias: profile.alias.clone(),
                })
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| {
        right
            .available
            .cmp(&left.available)
            .then_with(|| left.alias.cmp(&right.alias))
    });
    Ok(options)
}

fn oauth_profile_unavailable_reason(stored: &StoredProfile) -> Option<String> {
    if is_imported_oauth_profile(stored) {
        Some("JSON 导入账号用于反代账号池，不能用于登录态解锁".to_owned())
    } else if !stored.profile.enabled {
        Some("档案已停用".to_owned())
    } else if !stored.profile.credential_configured {
        Some("凭据未保存，请重新授权".to_owned())
    } else {
        None
    }
}

pub(crate) fn set_codex_oauth_profile(
    repository: &Repository,
    input: SetCodexGatewayOAuthProfileInput,
) -> AppResult<GatewayCodexConfigStatus> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let profile_id = input
        .profile_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(profile_id) = profile_id {
        let stored = repository.profile(&profile_id)?;
        if !is_codex_oauth_unlock_profile(&stored) {
            return Err(AppError::ProfileRuntimeUnavailable);
        }
        repository.set_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING, &profile_id)?;
    } else {
        repository.delete_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)?;
    }
    let path = config_path()?;
    status_for_path(repository, &path)
}

pub(crate) async fn codex_oauth_profile_auth_json(
    repository: &Repository,
    oauth_credentials: &OAuthCredentialStore,
    access: CredentialAccess,
) -> AppResult<Option<String>> {
    let profile_id = repository.setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)?;
    codex_oauth_profile_auth_json_for_id(
        repository,
        oauth_credentials,
        profile_id.as_deref(),
        access,
    )
    .await
}

async fn codex_oauth_profile_auth_json_for_id(
    repository: &Repository,
    oauth_credentials: &OAuthCredentialStore,
    profile_id: Option<&str>,
    access: CredentialAccess,
) -> AppResult<Option<String>> {
    let Some(profile_id) = profile_id else {
        return Ok(None);
    };
    let stored = repository.profile(profile_id)?;
    if !is_codex_oauth_unlock_profile(&stored) {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    let credential = oauth_credentials.load(&stored, access).await?;
    Ok(Some(credential.auth_json()?))
}

pub(crate) fn gateway_model_mappings(
    repository: &Repository,
) -> AppResult<Vec<GatewayModelMapping>> {
    let now = timestamp_ms();
    let mut mappings = repository
        .list_profiles()?
        .into_iter()
        .filter_map(|stored| {
            let profile = stored.profile;
            is_gateway_model_profile(&profile, now).then_some(profile)
        })
        .flat_map(|profile| {
            let mut mappings = profile.model_mappings;
            let mapped_models = mappings
                .iter()
                .map(|mapping| mapping.model.clone())
                .collect::<Vec<_>>();
            mappings.extend(profile.models.into_iter().filter_map(|model| {
                let trimmed = model.trim();
                (!trimmed.is_empty() && !mapped_models.iter().any(|candidate| candidate == trimmed))
                    .then(|| GatewayModelMapping {
                        model: model.clone(),
                        upstream_model: model,
                        display_name: None,
                        context_window: None,
                    })
            }));
            mappings
        })
        .filter(|mapping| !mapping.model.trim().is_empty())
        .collect::<Vec<_>>();
    mappings.sort_by(|left, right| left.model.cmp(&right.model));
    mappings.dedup_by(|left, right| left.model == right.model);
    if mappings.is_empty() {
        return Err(AppError::UpstreamUnavailable);
    }
    Ok(mappings)
}

pub(crate) fn gateway_model_options(repository: &Repository) -> AppResult<Vec<String>> {
    match gateway_model_mappings(repository) {
        Ok(mappings) => Ok(mappings.into_iter().map(|mapping| mapping.model).collect()),
        Err(AppError::UpstreamUnavailable) => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub(crate) fn gateway_model_is_available(repository: &Repository, model: &str) -> AppResult<bool> {
    Ok(gateway_model_options(repository)?
        .into_iter()
        .any(|candidate| candidate == model))
}

fn is_codex_oauth_unlock_profile(stored: &StoredProfile) -> bool {
    let profile = &stored.profile;
    profile.kind == ProfileKind::CodexOauth
        && profile.auth_mode == CodexAuthMode::OAuth
        && !is_imported_oauth_profile(stored)
        && profile.enabled
        && profile.credential_configured
}

fn is_imported_oauth_profile(stored: &StoredProfile) -> bool {
    stored.credential_fingerprint.is_some()
}

fn is_gateway_model_profile(profile: &crate::domain::MaskedProfile, now: i64) -> bool {
    profile.enabled
        && profile.in_pool
        && profile.credential_configured
        && (profile.kind == ProfileKind::ApiKey
            || (profile.kind == ProfileKind::CodexOauth
                && profile.auth_mode == CodexAuthMode::OAuth))
        && !matches!(
            profile.health.as_str(),
            "unhealthy" | "reauthorization_required"
        )
        && profile.cooldown_until_ms.is_none_or(|until| until <= now)
}

fn project_auth_json_to_default_codex_home(auth_json: &str) -> AppResult<()> {
    let home = config_path()?
        .parent()
        .ok_or(AppError::RuntimeUnavailable)?
        .to_path_buf();
    write_auth_json_to_home(&home, auth_json)?;
    project_desktop_auth_json(&home, auth_json)
}

fn write_auth_json_to_home(home: &Path, auth_json: &str) -> AppResult<()> {
    fs::create_dir_all(home).map_err(|_| AppError::RuntimeUnavailable)?;
    #[cfg(unix)]
    fs::set_permissions(home, fs::Permissions::from_mode(0o700))
        .map_err(|_| AppError::RuntimeUnavailable)?;
    let destination = home.join("auth.json");
    let temporary = home.join(format!(".auth-{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|_| AppError::RuntimeUnavailable)?;
    file.write_all(auth_json.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| AppError::RuntimeUnavailable)?;
    fs::rename(temporary, destination).map_err(|_| AppError::RuntimeUnavailable)
}

fn project_desktop_auth_json(codex_home: &Path, auth_json: &str) -> AppResult<()> {
    let _ = (codex_home, auth_json);
    Ok(())
}

pub(crate) async fn ensure_codex_client_key(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> AppResult<String> {
    if let Some(secret_ref) = repository.setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING)? {
        if client_key_secret_is_usable(repository, secrets.as_ref(), &secret_ref).await? {
            return Ok(secret_ref);
        }
    }
    create_codex_client_key(repository, secrets).await
}

async fn client_key_secret_is_usable(
    repository: &Repository,
    secrets: &dyn SecretStore,
    secret_ref: &str,
) -> AppResult<bool> {
    let Some((_, hash, _)) = repository
        .valid_key_hashes()?
        .into_iter()
        .find(|(_, _, candidate_ref)| candidate_ref == secret_ref)
    else {
        return Ok(false);
    };
    let Ok(token) = secrets.get(secret_ref).await else {
        return Ok(false);
    };
    let Ok(parsed) = PasswordHash::new(&hash) else {
        return Ok(false);
    };
    Ok(Argon2::default()
        .verify_password(token.as_bytes(), &parsed)
        .is_ok())
}

async fn create_codex_client_key(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> AppResult<String> {
    let id = Uuid::new_v4().to_string();
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let plaintext_once = format!("crl_{}", URL_SAFE_NO_PAD.encode(bytes));
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(plaintext_once.as_bytes(), &salt)
        .map_err(|_| AppError::Internal)?
        .to_string();
    let masked_value = format!(
        "crl_••••{}",
        &plaintext_once[plaintext_once.len().saturating_sub(4)..]
    );
    let secret_ref = format!("client-key:{id}");
    secrets.set(&secret_ref, &plaintext_once).await?;
    let key = MaskedClientKey {
        id,
        name: CODEX_CLIENT_KEY_NAME.to_owned(),
        masked_value,
        created_at_ms: timestamp_ms(),
        last_used_at_ms: None,
        revoked: false,
        managed_by: "codex_gateway".to_owned(),
        can_revoke: false,
    };
    if let Err(error) = repository.insert_client_key(&key, &hash, &secret_ref) {
        let _ = secrets.delete(&secret_ref).await;
        return Err(error);
    }
    if let Err(error) = repository.set_setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, &secret_ref) {
        let _ = repository.revoke_client_key(&key.id);
        let _ = secrets.delete(&secret_ref).await;
        return Err(error);
    }
    Ok(secret_ref)
}

pub async fn enable_api_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    id: &str,
    data_dir: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    enable_api_profile_at_path(repository, secrets, oauth_credentials, id, data_dir, &path).await
}

async fn enable_api_profile_at_path(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    id: &str,
    data_dir: &Path,
    path: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    let stored = repository.profile(id)?;
    if stored.profile.kind != crate::domain::ProfileKind::ApiKey
        || stored.profile.health != "healthy"
        || !stored.profile.enabled
        || !stored.profile.credential_configured
    {
        return Err(AppError::UpstreamUnavailable);
    }
    if !api_profile_supports_codex_direct(&stored.profile) {
        return Err(AppError::ValidationFailed);
    }
    let mappings = if stored.profile.model_mappings.is_empty() {
        identity_model_mappings(&stored.profile.models)
    } else {
        stored.profile.model_mappings.clone()
    };
    let model = mappings
        .first()
        .map(|mapping| mapping.upstream_model.clone())
        .ok_or(AppError::UpstreamUnavailable)?;
    let base_url = stored
        .profile
        .base_url
        .clone()
        .ok_or(AppError::UpstreamUnavailable)?;
    let auth_secret_ref = stored
        .secret_ref
        .as_deref()
        .ok_or(AppError::UpstreamUnavailable)?;
    let original = fs::read_to_string(path).unwrap_or_default();
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    let auth_json = codex_oauth_profile_auth_json_for_id(
        repository,
        &oauth_credentials,
        stored.profile.codex_oauth_profile_id.as_deref(),
        CredentialAccess::UserInitiated,
    )
    .await?;
    let service_url = Some(base_url.clone());
    let mut message = format!(
        "Codex 已切换到第三方模型提供商“{}”直连。请重启 Codex 以刷新模型列表。",
        stored.profile.alias
    );
    document["model_provider"] = value(CODEX_RELAY_DIRECT_PROVIDER);
    document["model"] = value(model);
    document["model_catalog_json"] = value(CODEX_RELAY_MODEL_CATALOG_FILENAME);
    let providers = document["model_providers"].or_insert(Item::Table(Table::new()));
    let providers = providers
        .as_table_like_mut()
        .ok_or(AppError::ValidationFailed)?;
    let mut provider = Table::new();
    provider["name"] = value(format!("Codex Relay · {}", stored.profile.alias));
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    let mut auth = Table::new();
    auth["command"] = value(
        std::env::current_exe()
            .map_err(|_| AppError::RuntimeUnavailable)?
            .display()
            .to_string(),
    );
    let mut args = Array::new();
    push_relay_auth_args(&mut args, auth_secret_ref, data_dir);
    auth["args"] = Item::Value(args.into());
    provider["auth"] = Item::Table(auth);
    providers.insert(CODEX_RELAY_DIRECT_PROVIDER, Item::Table(provider));
    let generated = document.to_string();
    store_config_snapshot(secrets, &original, &generated).await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
        write_direct_model_catalog(&parent.join(CODEX_RELAY_MODEL_CATALOG_FILENAME), &mappings)?;
    }
    fs::write(path, generated).map_err(|_| AppError::RuntimeUnavailable)?;
    repository.set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, &stored.profile.id)?;
    if let Some(auth_json) = auth_json.as_deref() {
        project_auth_json_to_default_codex_home(auth_json)?;
        message = format!("{message} OAuth 登录档案已同步到默认 Codex 凭据。");
    }
    let oauth_profile = codex_oauth_profile_status_for_id(
        repository,
        stored.profile.codex_oauth_profile_id.clone(),
    )?;
    Ok(GatewayCodexConfigStatus {
        enabled: true,
        mode: "third_party".to_owned(),
        config_path: path.display().to_string(),
        service_url,
        auth_status: "ok".to_owned(),
        needs_repair: false,
        message,
        direct_profile_id: Some(stored.profile.id.clone()),
        direct_profile_alias: Some(stored.profile.alias.clone()),
        oauth_profile_id: oauth_profile.id,
        oauth_profile_alias: oauth_profile.alias,
        oauth_profile_available: oauth_profile.available,
        oauth_profile_options: codex_oauth_profile_options(repository)?,
        history_sync: None,
        history_sync_status: None,
    })
}

fn api_profile_supports_codex_direct(profile: &crate::domain::MaskedProfile) -> bool {
    matches!(
        profile.provider,
        GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible
    ) && profile.wire_api == GatewayWireApi::Responses
}

fn identity_model_mappings(models: &[String]) -> Vec<GatewayModelMapping> {
    models
        .iter()
        .map(|model| GatewayModelMapping {
            model: model.clone(),
            upstream_model: model.clone(),
            display_name: None,
            context_window: None,
        })
        .collect()
}

fn write_model_catalog(path: &Path, mappings: &[GatewayModelMapping]) -> AppResult<()> {
    let catalog = json!({
        "models": mappings
            .iter()
            .enumerate()
            .map(|(index, mapping)| codex_catalog_entry(index, mapping))
            .collect::<Vec<_>>()
    });
    let text = serde_json::to_string_pretty(&catalog).map_err(|_| AppError::Internal)?;
    fs::write(path, text).map_err(|_| AppError::RuntimeUnavailable)
}

fn write_direct_model_catalog(path: &Path, mappings: &[GatewayModelMapping]) -> AppResult<()> {
    let catalog = json!({
        "models": mappings
            .iter()
            .enumerate()
            .map(|(index, mapping)| codex_direct_catalog_entry(index, mapping))
            .collect::<Vec<_>>()
    });
    let text = serde_json::to_string_pretty(&catalog).map_err(|_| AppError::Internal)?;
    fs::write(path, text).map_err(|_| AppError::RuntimeUnavailable)
}

fn codex_catalog_entry(index: usize, mapping: &GatewayModelMapping) -> Value {
    codex_catalog_entry_with_slug(index, mapping, &mapping.model)
}

fn codex_direct_catalog_entry(index: usize, mapping: &GatewayModelMapping) -> Value {
    codex_catalog_entry_with_slug(index, mapping, &mapping.upstream_model)
}

fn codex_catalog_entry_with_slug(index: usize, mapping: &GatewayModelMapping, slug: &str) -> Value {
    let display_name = mapping
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(mapping.model.as_str());
    let context_window = mapping
        .context_window
        .filter(|value| *value > 0)
        .unwrap_or(128_000);
    json!({
        "slug": slug,
        "display_name": display_name,
        "description": display_name,
        "base_instructions": "You are Codex, a coding agent. You and the user share the same workspace and collaborate to achieve the user's goals.",
        "default_reasoning_level": "high",
        "supported_reasoning_levels": [
            {"effort": "none", "description": "Disable Thinking"},
            {"effort": "high", "description": "Enabled Thinking"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1000 + index,
        "supports_reasoning_summaries": true,
        "default_reasoning_summary": "none",
        "support_verbosity": false,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": context_window,
        "max_context_window": context_window,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false
    })
}

pub async fn disable(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> AppResult<GatewayCodexConfigStatus> {
    restore_official_config(repository, secrets).await
}

pub async fn restore_official_config(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> AppResult<GatewayCodexConfigStatus> {
    let snapshot = secrets
        .get(SNAPSHOT_REF)
        .await
        .ok()
        .and_then(|value| serde_json::from_str::<ConfigSnapshot>(&value).ok());
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    let current = fs::read_to_string(&path).unwrap_or_default();
    if snapshot
        .as_ref()
        .is_some_and(|snapshot| content_hash(&current) == snapshot.generated_hash)
    {
        if let Some(snapshot) = snapshot {
            fs::write(&path, snapshot.original).map_err(|_| AppError::RuntimeUnavailable)?;
        }
    } else {
        let mut current_document = current
            .parse::<DocumentMut>()
            .map_err(|_| AppError::ValidationFailed)?;
        let original_document = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.original.parse::<DocumentMut>().ok())
            .unwrap_or_else(DocumentMut::new);
        remove_relay_config(&mut current_document, &original_document);
        fs::write(&path, current_document.to_string()).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    let _ = secrets.delete(SNAPSHOT_REF).await;
    repository.delete_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)?;
    let mut result = status(repository).await?;
    result.message = "已恢复官方 Codex 模型配置；保留了启用期间的其他 Codex 配置改动。".to_owned();
    Ok(result)
}

fn push_relay_auth_args(args: &mut Array, secret_ref: &str, data_dir: &Path) {
    args.push("--relay-gateway-token");
    args.push(secret_ref);
    args.push("--relay-data-dir");
    args.push(data_dir.display().to_string());
}

fn relay_auth_args(document: &DocumentMut, provider_name: &str) -> Option<Vec<String>> {
    document
        .get("model_providers")?
        .get(provider_name)?
        .get("auth")?
        .get("args")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

struct RelayAuthConfig {
    secret_ref: String,
    data_dir: PathBuf,
}

fn relay_auth_config(document: &DocumentMut, provider_name: &str) -> Option<RelayAuthConfig> {
    let args = relay_auth_args(document, provider_name)?;
    let secret_ref = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--relay-gateway-token").then(|| pair[1].clone()))?;
    let data_dir = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--relay-data-dir").then(|| PathBuf::from(&pair[1])))?;
    Some(RelayAuthConfig {
        secret_ref,
        data_dir,
    })
}

fn remove_relay_config(current: &mut DocumentMut, original: &DocumentMut) {
    let owns_provider = current
        .get("model_provider")
        .and_then(Item::as_str)
        .is_some_and(|provider| {
            provider == CODEX_RELAY_GATEWAY_PROVIDER || provider == CODEX_RELAY_DIRECT_PROVIDER
        });
    if owns_provider {
        if let Some(original_provider) = original.get("model_provider") {
            current["model_provider"] = original_provider.clone();
        } else {
            current.as_table_mut().remove("model_provider");
        }
        if let Some(original_model) = original.get("model") {
            current["model"] = original_model.clone();
        } else {
            current.as_table_mut().remove("model");
        }
    }
    if let Some(providers) = current
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove(CODEX_RELAY_GATEWAY_PROVIDER);
        providers.remove(CODEX_RELAY_DIRECT_PROVIDER);
    }
    let owns_catalog = current
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .and_then(|path| Path::new(path).file_name()?.to_str())
        == Some(CODEX_RELAY_MODEL_CATALOG_FILENAME);
    if owns_catalog {
        current.as_table_mut().remove("model_catalog_json");
    }
}

fn config_path() -> AppResult<PathBuf> {
    Ok(default_codex_home()?.join("config.toml"))
}

fn content_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };
    use toml_edit::DocumentMut;
    use uuid::Uuid;

    use super::{
        codex_oauth_profile_options, content_hash, enable_api_profile_at_path, enable_for_path,
        gateway_model_options, relay_auth_config, remove_relay_config, set_codex_oauth_profile,
        status_for_path, timestamp_ms,
    };
    use crate::{
        database::{Repository, StoredProfile},
        domain::{
            CodexAuthMode, GatewayModelMapping, GatewayNetworkAddress, GatewayProvider,
            GatewayStatus, GatewayWireApi, MaskedClientKey, MaskedProfile, ProfileKind,
            SetCodexGatewayOAuthProfileInput, GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
            GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
        },
        error::AppError,
        oauth_credentials::OAuthCredentialStore,
        secrets::{LocalEncryptedSecretStore, MemorySecretStore, SecretStore},
    };

    #[test]
    fn detects_config_changes_before_restore() {
        assert_ne!(
            content_hash("model_provider = 'one'"),
            content_hash("model_provider = 'two'")
        );
    }

    #[test]
    fn removes_only_relay_settings_when_config_changed_externally() {
        let original = "model_provider = 'openai'\nmodel = 'gpt-5'\nmodel_catalog_json = 'official-catalog.json'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut current = "model_provider = 'codex_relay_direct'\nmodel = 'provider-real'\nmodel_catalog_json = 'codex-relay-model-catalog.json'\n[model_providers.codex_relay]\nbase_url = 'https://relay'\n[model_providers.codex_relay_direct]\nbase_url = 'https://api.example.com/v1'\n[custom]\nvalue = 'keep'\n"
            .parse::<DocumentMut>()
            .unwrap();
        remove_relay_config(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("openai"));
        assert_eq!(current["model"].as_str(), Some("gpt-5"));
        assert!(current.get("model_catalog_json").is_none());
        assert!(current["model_providers"].get("codex_relay").is_none());
        assert!(current["model_providers"]
            .get("codex_relay_direct")
            .is_none());
        assert_eq!(current["custom"]["value"].as_str(), Some("keep"));
    }

    #[test]
    fn codex_oauth_profile_setting_round_trips_into_status() {
        let repository = Repository::memory();
        insert_oauth_profile(&repository, "oauth", "插件账号");

        let status = set_codex_oauth_profile(
            &repository,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("oauth".to_owned()),
                confirmed: true,
            },
        )
        .unwrap();

        assert_eq!(
            repository
                .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
                .unwrap()
                .as_deref(),
            Some("oauth")
        );
        assert_eq!(status.oauth_profile_id.as_deref(), Some("oauth"));
        assert_eq!(status.oauth_profile_alias.as_deref(), Some("插件账号"));
        assert!(status.oauth_profile_available);
        assert_eq!(status.oauth_profile_options.len(), 1);
        assert_eq!(status.oauth_profile_options[0].id, "oauth");
        assert!(status.oauth_profile_options[0].available);

        let status = set_codex_oauth_profile(
            &repository,
            SetCodexGatewayOAuthProfileInput {
                profile_id: None,
                confirmed: true,
            },
        )
        .unwrap();
        assert!(repository
            .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
            .unwrap()
            .is_none());
        assert!(status.oauth_profile_id.is_none());
    }

    #[test]
    fn codex_oauth_profile_options_mark_unavailable_reasons() {
        let repository = Repository::memory();
        insert_oauth_profile(&repository, "available", "可用登录");
        insert_oauth_profile_with(&repository, "disabled", "停用登录", false, true);
        insert_oauth_profile_with(&repository, "missing", "未授权登录", true, false);
        insert_imported_oauth_profile(&repository, "json-import", "JSON 导入登录");

        let options = codex_oauth_profile_options(&repository).unwrap();

        assert_eq!(options.len(), 4);
        assert_eq!(options[0].id, "available");
        assert!(options[0].available);
        assert!(options[0].reason.is_none());

        let disabled = options
            .iter()
            .find(|option| option.id == "disabled")
            .unwrap();
        assert!(!disabled.available);
        assert_eq!(disabled.reason.as_deref(), Some("档案已停用"));

        let missing = options
            .iter()
            .find(|option| option.id == "missing")
            .unwrap();
        assert!(!missing.available);
        assert_eq!(missing.reason.as_deref(), Some("凭据未保存，请重新授权"));

        let imported = options
            .iter()
            .find(|option| option.id == "json-import")
            .unwrap();
        assert!(!imported.available);
        assert_eq!(
            imported.reason.as_deref(),
            Some("JSON 导入账号用于反代账号池，不能用于登录态解锁")
        );
        assert!(set_codex_oauth_profile(
            &repository,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("json-import".to_owned()),
                confirmed: true,
            },
        )
        .is_err());
    }

    #[test]
    fn gateway_model_options_include_mappings_and_skip_unavailable_profiles() {
        let repository = Repository::memory();
        insert_api_profile(
            &repository,
            "api-healthy",
            "Visible API",
            "healthy",
            None,
            vec!["raw-api"],
            vec![GatewayModelMapping {
                model: "mapped-api".to_owned(),
                upstream_model: "provider-real".to_owned(),
                display_name: None,
                context_window: None,
            }],
        );
        insert_api_profile(
            &repository,
            "api-unhealthy",
            "Hidden API",
            "unhealthy",
            None,
            vec!["hidden-api"],
            Vec::new(),
        );
        insert_api_profile(
            &repository,
            "api-cooling",
            "Cooling API",
            "healthy",
            Some(timestamp_ms() + 60_000),
            vec!["cooling-api"],
            Vec::new(),
        );

        let models = gateway_model_options(&repository).unwrap();

        assert_eq!(models, vec!["mapped-api".to_owned(), "raw-api".to_owned()]);
    }

    #[test]
    fn gateway_model_options_returns_empty_when_no_profiles_are_available() {
        let repository = Repository::memory();

        let models = gateway_model_options(&repository).unwrap();

        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn api_profile_activation_writes_model_catalog_and_config_reference() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let stored = StoredProfile {
            profile: MaskedProfile {
                id: "api".to_owned(),
                alias: "Third Party".to_owned(),
                kind: ProfileKind::ApiKey,
                base_url: Some("https://api.example.com/v1".to_owned()),
                provider: GatewayProvider::OpenAiCompatible,
                wire_api: GatewayWireApi::Responses,
                enabled: true,
                in_pool: false,
                priority: 0,
                weight: 1,
                models: vec!["codex-visible".to_owned()],
                model_mappings: vec![GatewayModelMapping {
                    model: "codex-visible".to_owned(),
                    upstream_model: "provider-real".to_owned(),
                    display_name: Some("Provider Real".to_owned()),
                    context_window: Some(64_000),
                }],
                health: "healthy".to_owned(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
            },
            secret_ref: Some("profile:api:credential".to_owned()),
            credential_fingerprint: None,
        };
        repository.insert_profile(&stored).unwrap();
        secrets
            .set("profile:api:credential", "sk-test")
            .await
            .unwrap();
        let root = temp_root("codex-gateway-api-profile");
        let data_dir = root.join("data");
        let config_path = root.join("home/.codex/config.toml");

        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        let result = enable_api_profile_at_path(
            &repository,
            secrets.clone(),
            oauth_credentials,
            "api",
            &data_dir,
            &config_path,
        )
        .await
        .unwrap();

        let document = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(result.enabled);
        assert_eq!(result.mode, "third_party");
        assert_eq!(
            result.service_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(result.direct_profile_id.as_deref(), Some("api"));
        assert_eq!(result.direct_profile_alias.as_deref(), Some("Third Party"));
        assert_eq!(
            document["model_provider"].as_str(),
            Some("codex_relay_direct")
        );
        assert_eq!(document["model"].as_str(), Some("provider-real"));
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some("codex-relay-model-catalog.json")
        );
        assert_eq!(
            document["model_providers"]["codex_relay_direct"]["base_url"].as_str(),
            Some("https://api.example.com/v1")
        );
        let auth = relay_auth_config(&document, "codex_relay_direct").unwrap();
        assert_eq!(auth.secret_ref, "profile:api:credential");
        assert_eq!(auth.data_dir, data_dir);
        assert!(!repository.profile("api").unwrap().profile.in_pool);
        assert_eq!(
            repository
                .setting(crate::domain::GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)
                .unwrap()
                .as_deref(),
            Some("api")
        );
        let catalog_path = root.join("home/.codex/codex-relay-model-catalog.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(catalog_path).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["slug"], "provider-real");
        assert_eq!(catalog["models"][0]["display_name"], "Provider Real");
        assert_eq!(catalog["models"][0]["context_window"], 64_000);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn api_profile_activation_rejects_non_responses_direct_profiles() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let stored = StoredProfile {
            profile: MaskedProfile {
                id: "api".to_owned(),
                alias: "Chat Provider".to_owned(),
                kind: ProfileKind::ApiKey,
                base_url: Some("https://api.example.com/v1".to_owned()),
                provider: GatewayProvider::OpenAiCompatible,
                wire_api: GatewayWireApi::ChatCompletions,
                enabled: true,
                in_pool: false,
                priority: 0,
                weight: 1,
                models: vec!["chat-model".to_owned()],
                model_mappings: Vec::new(),
                health: "healthy".to_owned(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
            },
            secret_ref: Some("profile:api:credential".to_owned()),
            credential_fingerprint: None,
        };
        repository.insert_profile(&stored).unwrap();
        let root = temp_root("codex-gateway-api-profile-reject");
        let data_dir = root.join("data");
        let config_path = root.join("home/.codex/config.toml");
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));

        let error = enable_api_profile_at_path(
            &repository,
            secrets,
            oauth_credentials,
            "api",
            &data_dir,
            &config_path,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AppError::ValidationFailed));
        assert!(!config_path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn enable_repairs_a_missing_codex_client_key_secret() {
        let repository = Repository::memory();
        insert_api_profile(
            &repository,
            "api-healthy",
            "Visible API",
            "healthy",
            None,
            vec!["codex-visible"],
            Vec::new(),
        );
        insert_stale_client_key(&repository);
        repository
            .set_setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, "client-key:stale")
            .unwrap();
        let root = temp_root("codex-gateway-enable-repair");
        let data_dir = root.join("data");
        let secrets = Arc::new(LocalEncryptedSecretStore::open(&data_dir).unwrap());
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        let config_path = root.join("home/.codex/config.toml");

        let result = enable_for_path(
            &repository,
            secrets,
            oauth_credentials,
            running_gateway(),
            &data_dir,
            &config_path,
        )
        .await
        .unwrap();

        let document = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let auth = relay_auth_config(&document, "codex_relay").unwrap();
        let token = crate::read_relay_gateway_token(&auth.secret_ref, &auth.data_dir).unwrap();

        assert!(result.enabled);
        assert_eq!(result.auth_status, "ok");
        assert!(!result.needs_repair);
        assert_eq!(document["model_provider"].as_str(), Some("codex_relay"));
        assert_ne!(auth.secret_ref, "client-key:stale");
        assert_eq!(
            repository
                .setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING)
                .unwrap()
                .as_deref(),
            Some(auth.secret_ref.as_str())
        );
        assert!(token.starts_with("crl_"));
        assert!(repository
            .valid_key_hashes()
            .unwrap()
            .iter()
            .any(|(_, _, secret_ref)| secret_ref == &auth.secret_ref));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn status_reports_an_enabled_relay_with_a_missing_auth_secret() {
        let repository = Repository::memory();
        let root = temp_root("codex-gateway-status-missing-secret");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        LocalEncryptedSecretStore::open(&data_dir).unwrap();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
model_provider = "codex_relay"

[model_providers.codex_relay]
name = "Codex Relay LAN Gateway"
base_url = "https://10.12.14.248:53765/v1"
wire_api = "responses"

[model_providers.codex_relay.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "client-key:missing", "--relay-data-dir", "{}"]
"#,
                data_dir.display()
            ),
        )
        .unwrap();

        let status = status_for_path(&repository, &config_path).unwrap();

        assert!(status.enabled);
        assert_eq!(status.auth_status, "invalid");
        assert!(status.needs_repair);
        assert!(status.message.contains("Client Key 已失效"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn insert_stale_client_key(repository: &Repository) {
        let plaintext = "crl_stale_secret";
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(plaintext.as_bytes(), &salt)
            .unwrap()
            .to_string();
        let key = MaskedClientKey {
            id: "stale".to_owned(),
            name: "测试".to_owned(),
            masked_value: "crl_••••cret".to_owned(),
            created_at_ms: 1,
            last_used_at_ms: None,
            revoked: false,
            managed_by: "user".to_owned(),
            can_revoke: true,
        };
        repository
            .insert_client_key(&key, &hash, "client-key:stale")
            .unwrap();
    }

    fn insert_oauth_profile(repository: &Repository, id: &str, alias: &str) {
        insert_oauth_profile_with(repository, id, alias, true, true);
    }

    fn insert_oauth_profile_with(
        repository: &Repository,
        id: &str,
        alias: &str,
        enabled: bool,
        credential_configured: bool,
    ) {
        repository
            .insert_profile(&StoredProfile {
                profile: MaskedProfile {
                    id: id.to_owned(),
                    alias: alias.to_owned(),
                    kind: ProfileKind::CodexOauth,
                    base_url: None,
                    provider: Default::default(),
                    wire_api: Default::default(),
                    enabled,
                    in_pool: false,
                    priority: 0,
                    weight: 1,
                    models: Vec::new(),
                    model_mappings: Vec::new(),
                    health: "healthy".to_owned(),
                    cooldown_until_ms: None,
                    credential_configured,
                    auth_mode: CodexAuthMode::OAuth,
                    codex_oauth_profile_id: None,
                    is_current: false,
                    account: None,
                },
                secret_ref: credential_configured.then(|| format!("profile:{id}:oauth")),
                credential_fingerprint: None,
            })
            .unwrap();
    }

    fn insert_imported_oauth_profile(repository: &Repository, id: &str, alias: &str) {
        repository
            .insert_profile(&StoredProfile {
                profile: MaskedProfile {
                    id: id.to_owned(),
                    alias: alias.to_owned(),
                    kind: ProfileKind::CodexOauth,
                    base_url: None,
                    provider: Default::default(),
                    wire_api: Default::default(),
                    enabled: true,
                    in_pool: true,
                    priority: 0,
                    weight: 1,
                    models: vec!["imported-codex".to_owned()],
                    model_mappings: Vec::new(),
                    health: "healthy".to_owned(),
                    cooldown_until_ms: None,
                    credential_configured: true,
                    auth_mode: CodexAuthMode::OAuth,
                    codex_oauth_profile_id: None,
                    is_current: false,
                    account: None,
                },
                secret_ref: Some(format!("profile:{id}:oauth")),
                credential_fingerprint: Some(format!("json:{id}")),
            })
            .unwrap();
    }

    fn insert_api_profile(
        repository: &Repository,
        id: &str,
        alias: &str,
        health: &str,
        cooldown_until_ms: Option<i64>,
        models: Vec<&str>,
        model_mappings: Vec<GatewayModelMapping>,
    ) {
        repository
            .insert_profile(&StoredProfile {
                profile: MaskedProfile {
                    id: id.to_owned(),
                    alias: alias.to_owned(),
                    kind: ProfileKind::ApiKey,
                    base_url: Some("https://api.example.com/v1".to_owned()),
                    provider: GatewayProvider::OpenAiCompatible,
                    wire_api: GatewayWireApi::Responses,
                    enabled: true,
                    in_pool: true,
                    priority: 0,
                    weight: 1,
                    models: models.into_iter().map(ToOwned::to_owned).collect(),
                    model_mappings,
                    health: health.to_owned(),
                    cooldown_until_ms,
                    credential_configured: true,
                    auth_mode: CodexAuthMode::OAuth,
                    codex_oauth_profile_id: None,
                    is_current: false,
                    account: None,
                },
                secret_ref: Some(format!("profile:{id}:credential")),
                credential_fingerprint: None,
            })
            .unwrap();
    }

    fn running_gateway() -> GatewayStatus {
        GatewayStatus {
            running: true,
            bind_mode: "lan".to_owned(),
            bind_address: "10.12.14.248".to_owned(),
            available_addresses: vec![GatewayNetworkAddress {
                name: "en0".to_owned(),
                address: "10.12.14.248".to_owned(),
                is_default: true,
            }],
            port: 53765,
            cidrs: Vec::new(),
            available_profiles: 1,
            cooling_profiles: 0,
            client_key_count: 1,
            certificate_ready: true,
            service_url: "https://10.12.14.248:53765".to_owned(),
            upstream_proxy_mode: "system".to_owned(),
            upstream_proxy_display: None,
            upstream_last_error: None,
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
}
