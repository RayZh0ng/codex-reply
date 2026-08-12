use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
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
    profiles::{self, timestamp_ms},
    secrets::SecretStore,
};

const SNAPSHOT_REF: &str = "gateway:codex-config-snapshot";
const CODEX_CLIENT_KEY_NAME: &str = "Codex CLI Gateway";
const CODEX_RELAY_MODEL_CATALOG_FILENAME: &str = "codex-relay-model-catalog.json";
const CODEX_RELAY_GATEWAY_PROVIDER: &str = "codex_relay";
const CODEX_RELAY_DIRECT_PROVIDER: &str = "codex_relay_direct";
const CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY: &str = "cli_auth_credentials_store";
const CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE: &str = "file";
const CODEX_APP_SERVER_ACCOUNT_READ_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Serialize, Deserialize)]
struct ConfigSnapshot {
    original: String,
    generated_hash: String,
}

pub(crate) struct ManagedCodexConfigSnapshot {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    stored_snapshot: Option<String>,
}

trait ProjectedOAuthIdentityVerifier: Send + Sync {
    fn verify(&self, home: &Path, expected_account_id: &str) -> AppResult<()>;
}

struct AppServerProjectedOAuthIdentityVerifier;

impl ProjectedOAuthIdentityVerifier for AppServerProjectedOAuthIdentityVerifier {
    fn verify(&self, home: &Path, expected_account_id: &str) -> AppResult<()> {
        let actual = read_codex_account_id_from_app_server(home)?;
        match actual.as_deref() {
            Some(account_id) if account_id == expected_account_id => Ok(()),
            Some(_) | None => Err(AppError::OAuthIdentityMismatch),
        }
    }
}

#[cfg(test)]
struct AuthJsonProjectedOAuthIdentityVerifier;

#[cfg(test)]
impl ProjectedOAuthIdentityVerifier for AuthJsonProjectedOAuthIdentityVerifier {
    fn verify(&self, home: &Path, expected_account_id: &str) -> AppResult<()> {
        let auth_json =
            fs::read_to_string(home.join("auth.json")).map_err(|_| AppError::RuntimeUnavailable)?;
        match auth_json_account_id(&auth_json)?.as_deref() {
            Some(account_id) if account_id == expected_account_id => Ok(()),
            Some(_) | None => Err(AppError::OAuthIdentityMismatch),
        }
    }
}

#[cfg(test)]
struct MismatchedProjectedOAuthIdentityVerifier;

#[cfg(test)]
impl ProjectedOAuthIdentityVerifier for MismatchedProjectedOAuthIdentityVerifier {
    fn verify(&self, _home: &Path, _expected_account_id: &str) -> AppResult<()> {
        Err(AppError::OAuthIdentityMismatch)
    }
}

pub async fn status(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    repair_managed_config_at_path(Some(secrets), &path).await?;
    status_for_path(repository, &path)
}

pub(crate) async fn snapshot_managed_config(
    secrets: Arc<dyn SecretStore>,
) -> AppResult<ManagedCodexConfigSnapshot> {
    let config_path = config_path()?;
    snapshot_managed_config_at_path(secrets, &config_path).await
}

pub(crate) async fn snapshot_managed_config_at_path(
    secrets: Arc<dyn SecretStore>,
    config_path: &Path,
) -> AppResult<ManagedCodexConfigSnapshot> {
    let home = config_path
        .parent()
        .ok_or(AppError::RuntimeUnavailable)?
        .to_path_buf();
    let paths = [
        config_path.to_path_buf(),
        home.join(CODEX_RELAY_MODEL_CATALOG_FILENAME),
        home.join("auth.json"),
    ];
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let contents = match fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(_) => return Err(AppError::RuntimeUnavailable),
        };
        files.push((path, contents));
    }
    let stored_snapshot = match secrets.get(SNAPSHOT_REF).await {
        Ok(snapshot) => Some(snapshot),
        Err(AppError::NotFound) => None,
        Err(error) => return Err(error),
    };
    Ok(ManagedCodexConfigSnapshot {
        files,
        stored_snapshot,
    })
}

pub(crate) async fn restore_managed_config(
    secrets: Arc<dyn SecretStore>,
    snapshot: ManagedCodexConfigSnapshot,
) -> AppResult<()> {
    for (path, contents) in snapshot.files {
        match contents {
            Some(contents) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
                }
                fs::write(path, contents).map_err(|_| AppError::RuntimeUnavailable)?;
            }
            None => match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(_) => return Err(AppError::RuntimeUnavailable),
            },
        }
    }
    match snapshot.stored_snapshot {
        Some(stored_snapshot) => secrets.set(SNAPSHOT_REF, &stored_snapshot).await,
        None => match secrets.delete(SNAPSHOT_REF).await {
            Ok(()) | Err(AppError::NotFound) => Ok(()),
            Err(error) => Err(error),
        },
    }
}

fn restore_setting(repository: &Repository, key: &str, value: Option<String>) {
    let _ = match value {
        Some(value) => repository.set_setting(key, &value),
        None => repository.delete_setting(key),
    };
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
    let catalog_issue = enabled
        .then(|| relay_model_catalog_issue(document.as_ref(), path))
        .flatten();
    let auth_storage_issue = enabled
        .then(|| relay_auth_storage_issue(document.as_ref()))
        .flatten();
    let auth_config = document
        .as_ref()
        .and_then(|document| relay_auth_config(document, provider.as_deref()?));
    let bridge_config =
        mode == "third_party" && document.as_ref().is_some_and(direct_oauth_bridge_config);
    let configured_service_url = document.as_ref().and_then(|document| {
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
    let auth_status = if bridge_config {
        "ok"
    } else if enabled && auth_config.is_none() {
        "legacy"
    } else if enabled && !auth_token_readable {
        "invalid"
    } else if enabled {
        "ok"
    } else {
        "missing"
    };
    let direct_profile = direct_profile_status(repository)?;
    let service_url = if mode == "third_party" {
        direct_profile.service_url.clone()
    } else {
        configured_service_url
    };
    let oauth_profile = if mode == "third_party" {
        let profile_id = direct_profile
            .id
            .as_deref()
            .and_then(|id| repository.profile(id).ok())
            .and_then(|stored| stored.profile.codex_oauth_profile_id);
        codex_oauth_profile_status_for_id(repository, profile_id)?
    } else {
        codex_oauth_profile_status(repository)?
    };
    let oauth_profile_options = codex_oauth_profile_options(repository)?;
    Ok(GatewayCodexConfigStatus {
        enabled,
        mode: mode.to_owned(),
        config_path: path.display().to_string(),
        service_url,
        auth_status: auth_status.to_owned(),
        needs_repair: matches!(auth_status, "legacy" | "invalid")
            || catalog_issue.is_some()
            || auth_storage_issue.is_some(),
        message: status_message(
            mode,
            bridge_config,
            auth_config.is_some(),
            auth_token_readable,
            &direct_profile,
            catalog_issue.as_deref(),
            auth_storage_issue.as_deref(),
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
    service_url: Option<String>,
}

#[derive(Debug, Clone)]
struct ThirdPartyOAuthProjection {
    applied: bool,
    message: String,
}

#[derive(Clone, Copy)]
struct CodexOAuthProfileSwitchContext<'a> {
    data_dir: &'a Path,
    gateway: Option<&'a GatewayStatus>,
    path: &'a Path,
    project_desktop: bool,
}

fn direct_profile_status(repository: &Repository) -> AppResult<DirectProfileStatus> {
    let Some(profile_id) = repository.setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)? else {
        return Ok(DirectProfileStatus {
            id: None,
            alias: None,
            service_url: None,
        });
    };
    let stored = repository.profile(&profile_id).ok();
    let alias = stored.as_ref().map(|stored| stored.profile.alias.clone());
    let service_url = stored.and_then(|stored| stored.profile.base_url);
    Ok(DirectProfileStatus {
        id: Some(profile_id),
        alias,
        service_url,
    })
}

fn status_message(
    mode: &str,
    bridge_config: bool,
    has_auth_config: bool,
    auth_token_readable: bool,
    direct_profile: &DirectProfileStatus,
    catalog_issue: Option<&str>,
    auth_storage_issue: Option<&str>,
) -> String {
    if mode == "official" {
        return "Codex 正在使用官方模型配置。".to_owned();
    }
    if let Some(catalog_issue) = catalog_issue {
        return catalog_issue.to_owned();
    }
    if let Some(auth_storage_issue) = auth_storage_issue {
        return auth_storage_issue.to_owned();
    }
    if bridge_config {
        return match direct_profile.alias.as_deref() {
            Some(alias) => format!(
                "Codex 正通过 codex_relay_direct 经本机 Relay 固定转发到第三方供应商“{alias}”，ChatGPT 登录使用所选 OAuth 档案。"
            ),
            None => "Codex 第三方 OAuth 桥接配置缺少对应档案，请重新应用第三方配置。"
                .to_owned(),
        };
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

fn relay_model_catalog_path(config_path: &Path) -> AppResult<PathBuf> {
    config_path
        .parent()
        .map(|parent| parent.join(CODEX_RELAY_MODEL_CATALOG_FILENAME))
        .ok_or(AppError::RuntimeUnavailable)
}

fn repair_legacy_model_catalog_path(
    document: &mut DocumentMut,
    config_path: &Path,
) -> AppResult<bool> {
    if document.get("model_catalog_json").and_then(Item::as_str)
        != Some(CODEX_RELAY_MODEL_CATALOG_FILENAME)
    {
        return Ok(false);
    }
    let catalog_path = relay_model_catalog_path(config_path)?;
    document["model_catalog_json"] = value(catalog_path.display().to_string());
    Ok(true)
}

fn repair_provider_auth_conflict(document: &mut DocumentMut, provider_name: &str) -> bool {
    let Some(provider) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut(provider_name))
        .and_then(Item::as_table_like_mut)
    else {
        return false;
    };
    if provider.get("auth").is_none() || provider.get("requires_openai_auth").is_none() {
        return false;
    }
    provider.remove("requires_openai_auth");
    true
}

fn repair_managed_provider_auth_conflicts(document: &mut DocumentMut) -> bool {
    let mut changed = false;
    for provider_name in [CODEX_RELAY_GATEWAY_PROVIDER, CODEX_RELAY_DIRECT_PROVIDER] {
        changed |= repair_provider_auth_conflict(document, provider_name);
    }
    changed
}

fn relay_auth_storage_issue(document: Option<&DocumentMut>) -> Option<String> {
    let storage = document?.get(CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY);
    (storage.and_then(Item::as_str) != Some(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE)).then(|| {
        "Codex 登录凭据存储未固定为 auth.json 文件；请重新应用当前 Relay 或第三方供应商配置。"
            .to_owned()
    })
}

fn ensure_relay_file_auth_storage(document: &mut DocumentMut) -> bool {
    if document
        .get(CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY)
        .and_then(Item::as_str)
        == Some(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE)
    {
        return false;
    }
    document[CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY] = value(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE);
    true
}

fn direct_oauth_bridge_config(document: &DocumentMut) -> bool {
    document
        .get("model_providers")
        .and_then(|providers| providers.get(CODEX_RELAY_DIRECT_PROVIDER))
        .and_then(Item::as_table_like)
        .is_some_and(|provider| {
            provider.get("requires_openai_auth").and_then(Item::as_bool) == Some(true)
                && provider.get("auth").is_none()
        })
}

async fn repair_managed_config_at_path(
    secrets: Option<Arc<dyn SecretStore>>,
    path: &Path,
) -> AppResult<bool> {
    let original = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(AppError::RuntimeUnavailable),
    };
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    let active_provider = document.get("model_provider").and_then(Item::as_str);
    let owns_active_provider = matches!(
        active_provider,
        Some(CODEX_RELAY_GATEWAY_PROVIDER | CODEX_RELAY_DIRECT_PROVIDER)
    );
    let mut changed = false;
    if owns_active_provider {
        changed |= repair_managed_provider_auth_conflicts(&mut document);
        changed |= repair_legacy_model_catalog_path(&mut document, path)?;
        changed |= ensure_relay_file_auth_storage(&mut document);
    }
    if !changed {
        return Ok(false);
    }

    let repaired = document.to_string();
    let owned_snapshot = if let Some(secrets) = secrets.as_ref() {
        match secrets.get(SNAPSHOT_REF).await {
            Ok(value) => serde_json::from_str::<ConfigSnapshot>(&value)
                .ok()
                .filter(|snapshot| content_hash(&original) == snapshot.generated_hash),
            Err(AppError::NotFound) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    fs::write(path, &repaired).map_err(|_| AppError::RuntimeUnavailable)?;
    if let (Some(secrets), Some(snapshot)) = (secrets, owned_snapshot) {
        let rebased = ConfigSnapshot {
            original: snapshot.original,
            generated_hash: content_hash(&repaired),
        };
        let serialized = serde_json::to_string(&rebased).map_err(|_| AppError::Internal)?;
        if let Err(error) = secrets.set(SNAPSHOT_REF, &serialized).await {
            fs::write(path, original).map_err(|_| AppError::RuntimeUnavailable)?;
            return Err(error);
        }
    }
    Ok(true)
}

fn relay_model_catalog_issue(document: Option<&DocumentMut>, config_path: &Path) -> Option<String> {
    let configured = document?
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .map(PathBuf::from);
    let Some(configured) = configured else {
        return Some(
            "Codex 模型目录配置缺失；请重新应用当前 Relay 或第三方供应商配置。".to_owned(),
        );
    };
    if !configured.is_absolute() {
        return Some(
            "Codex 模型目录仍使用相对路径；请重新应用当前 Relay 或第三方供应商配置。".to_owned(),
        );
    }
    if !configured.exists() {
        let expected = relay_model_catalog_path(config_path).ok();
        return Some(match expected {
            Some(expected) if expected == configured => {
                "Codex 模型目录文件不存在；请重新应用当前 Relay 或第三方供应商配置。".to_owned()
            }
            _ => "Codex 模型目录路径无效；请重新应用当前 Relay 或第三方供应商配置。".to_owned(),
        });
    }
    None
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
    let verifier = AppServerProjectedOAuthIdentityVerifier;
    enable_for_path_with_verifier(
        repository,
        secrets,
        oauth_credentials,
        gateway,
        data_dir,
        path,
        &verifier,
    )
    .await
}

async fn enable_for_path_with_verifier(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
    path: &Path,
    verifier: &dyn ProjectedOAuthIdentityVerifier,
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
    let config_snapshot = snapshot_managed_config_at_path(secrets.clone(), path).await?;
    let previous_direct_setting = repository.setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)?;
    let result: AppResult<GatewayCodexConfigStatus> = async {
        let mut status = enable_at_path(
            secrets.clone(),
            gateway,
            data_dir,
            path,
            &secret_ref,
            &model_mappings,
        )
        .await?;
        repository.delete_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)?;
        if let Some(auth_json) = auth_json.as_deref() {
            let projection_message =
                project_auth_json_to_default_codex_home_with_verifier(auth_json, verifier)?;
            status.message = format!("{} {}", status.message, projection_message);
        }
        let oauth = codex_oauth_profile_status(repository)?;
        status.oauth_profile_id = oauth.id;
        status.oauth_profile_alias = oauth.alias;
        status.oauth_profile_available = oauth.available;
        status.oauth_profile_options = codex_oauth_profile_options(repository)?;
        Ok(status)
    }
    .await;

    match result {
        Ok(status) => Ok(status),
        Err(error) => {
            let _ = restore_managed_config(secrets, config_snapshot).await;
            restore_setting(
                repository,
                GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING,
                previous_direct_setting,
            );
            Err(error)
        }
    }
}

async fn enable_at_path(
    secrets: Arc<dyn SecretStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
    path: &Path,
    secret_ref: &str,
    model_mappings: &[GatewayModelMapping],
) -> AppResult<GatewayCodexConfigStatus> {
    let catalog_path = relay_model_catalog_path(path)?;
    let original = fs::read_to_string(path).unwrap_or_default();
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    ensure_relay_file_auth_storage(&mut document);
    document["model_provider"] = value(CODEX_RELAY_GATEWAY_PROVIDER);
    if let Some(model) = model_mappings.first() {
        document["model"] = value(model.model.clone());
    }
    document["model_catalog_json"] = value(catalog_path.display().to_string());
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
    repair_managed_provider_auth_conflicts(&mut document);
    let generated = document.to_string();
    store_config_snapshot(secrets.clone(), &original, &generated).await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
        write_model_catalog(&catalog_path, model_mappings)?;
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
    } else if stored.profile.validation_status == "invalid" {
        Some(
            stored
                .profile
                .validation_message
                .clone()
                .unwrap_or_else(|| "档案验证已失效，请重新授权".to_owned()),
        )
    } else {
        None
    }
}

pub(crate) async fn set_codex_oauth_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: &OAuthCredentialStore,
    input: SetCodexGatewayOAuthProfileInput,
    data_dir: &Path,
    gateway: Option<GatewayStatus>,
) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    set_codex_oauth_profile_at_path(
        repository,
        secrets,
        oauth_credentials,
        input,
        CodexOAuthProfileSwitchContext {
            data_dir,
            gateway: gateway.as_ref(),
            path: &path,
            project_desktop: true,
        },
    )
    .await
}

async fn set_codex_oauth_profile_at_path(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: &OAuthCredentialStore,
    input: SetCodexGatewayOAuthProfileInput,
    context: CodexOAuthProfileSwitchContext<'_>,
) -> AppResult<GatewayCodexConfigStatus> {
    let verifier = AppServerProjectedOAuthIdentityVerifier;
    set_codex_oauth_profile_at_path_with_verifier(
        repository,
        secrets,
        oauth_credentials,
        input,
        context,
        &verifier,
    )
    .await
}

async fn set_codex_oauth_profile_at_path_with_verifier(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: &OAuthCredentialStore,
    input: SetCodexGatewayOAuthProfileInput,
    context: CodexOAuthProfileSwitchContext<'_>,
    verifier: &dyn ProjectedOAuthIdentityVerifier,
) -> AppResult<GatewayCodexConfigStatus> {
    let CodexOAuthProfileSwitchContext {
        data_dir,
        gateway,
        path,
        project_desktop,
    } = context;
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let config_snapshot = snapshot_managed_config_at_path(secrets.clone(), path).await?;
    let previous_oauth_setting = repository.setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)?;
    let mut previous_direct_profile: Option<StoredProfile> = None;
    let result: AppResult<GatewayCodexConfigStatus> = async {
        repair_managed_config_at_path(Some(secrets.clone()), path).await?;
        let current = status_for_path(repository, path)?;
        let profile_id = input
            .profile_id
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if let Some(profile_id) = profile_id.as_deref() {
            let stored = repository.profile(profile_id)?;
            let valid_profile = if current.mode == "third_party" {
                is_codex_oauth_profile_candidate(&stored)
            } else {
                is_codex_oauth_unlock_profile(&stored)
            };
            if !valid_profile {
                return Err(AppError::ProfileRuntimeUnavailable);
            }
        }
        if current.mode == "third_party" {
            let direct_profile_id = current
                .direct_profile_id
                .as_deref()
                .ok_or(AppError::NotFound)?;
            previous_direct_profile = Some(repository.profile(direct_profile_id)?);
        }
        let auth_json = if current.enabled && current.mode != "third_party" {
            codex_oauth_profile_auth_json_for_id(
                repository,
                oauth_credentials,
                profile_id.as_deref(),
                CredentialAccess::UserInitiated,
            )
            .await?
        } else {
            None
        };
        if let Some(previous_direct_profile) = previous_direct_profile.as_ref() {
            let mut stored = previous_direct_profile.clone();
            if stored.profile.kind != ProfileKind::ApiKey {
                return Err(AppError::ValidationFailed);
            }
            stored.profile.codex_oauth_profile_id = profile_id.clone();
            repository.update_profile(&stored)?;
        } else if let Some(profile_id) = profile_id.as_deref() {
            repository.set_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING, profile_id)?;
        } else {
            repository.delete_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)?;
        }
        if current.enabled && current.mode == "third_party" {
            let direct_profile_id = previous_direct_profile
                .as_ref()
                .map(|stored| stored.profile.id.as_str())
                .ok_or(AppError::NotFound)?;
            return enable_api_profile_at_path(
                repository,
                secrets.clone(),
                oauth_credentials,
                direct_profile_id,
                data_dir,
                gateway,
                path,
            )
            .await;
        }
        let projection_message = if let Some(auth_json) = auth_json.as_deref() {
            let home = path.parent().ok_or(AppError::RuntimeUnavailable)?;
            Some(project_auth_json_to_home_with_verifier(
                home,
                auth_json,
                project_desktop,
                verifier,
            )?)
        } else {
            None
        };
        let mut status = status_for_path(repository, path)?;
        if let Some(message) = projection_message {
            status.message = format!("{} {}", status.message, message);
        }
        Ok(status)
    }
    .await;

    match result {
        Ok(status) => Ok(status),
        Err(error) => {
            let _ = restore_managed_config(secrets, config_snapshot).await;
            if let Some(previous_direct_profile) = previous_direct_profile {
                let _ = repository.update_profile(&previous_direct_profile);
            } else {
                restore_setting(
                    repository,
                    GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
                    previous_oauth_setting,
                );
            }
            Err(error)
        }
    }
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
    let credential = oauth_credentials
        .current_or_refresh(&stored, access)
        .await?;
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

#[cfg(test)]
async fn sync_active_direct_oauth_profile_at_path(
    repository: &Repository,
    oauth_credentials: &OAuthCredentialStore,
    profile_id: &str,
    path: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    repair_managed_config_at_path(None, path).await?;
    let current = status_for_path(repository, path)?;
    if !current.enabled
        || current.mode != "third_party"
        || current.direct_profile_id.as_deref() != Some(profile_id)
    {
        return Ok(current);
    }

    let stored = repository.profile(profile_id)?;
    if stored.profile.kind != ProfileKind::ApiKey {
        return Err(AppError::ValidationFailed);
    }
    let projection_message = project_third_party_oauth_profile_to_home(
        repository,
        oauth_credentials,
        stored.profile.codex_oauth_profile_id.as_deref(),
        path.parent(),
    )
    .await;

    let mut status = status_for_path(repository, path)?;
    status.message = format!("{} {}", status.message, projection_message.message);
    Ok(status)
}

fn is_codex_oauth_unlock_profile(stored: &StoredProfile) -> bool {
    let profile = &stored.profile;
    is_codex_oauth_profile_candidate(stored)
        && profile.enabled
        && profile.credential_configured
        && profile.validation_status != "invalid"
}

fn is_codex_oauth_profile_candidate(stored: &StoredProfile) -> bool {
    let profile = &stored.profile;
    profile.kind == ProfileKind::CodexOauth
        && profile.auth_mode == CodexAuthMode::OAuth
        && !is_imported_oauth_profile(stored)
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
        && profile.validation_status != "invalid"
        && profile.cooldown_until_ms.is_none_or(|until| until <= now)
}

fn project_auth_json_to_default_codex_home_with_verifier(
    auth_json: &str,
    verifier: &dyn ProjectedOAuthIdentityVerifier,
) -> AppResult<String> {
    let home = config_path()?
        .parent()
        .ok_or(AppError::RuntimeUnavailable)?
        .to_path_buf();
    project_auth_json_to_home_with_verifier(&home, auth_json, true, verifier)
}

async fn project_third_party_oauth_profile_to_home(
    repository: &Repository,
    oauth_credentials: &OAuthCredentialStore,
    profile_id: Option<&str>,
    home: Option<&Path>,
) -> ThirdPartyOAuthProjection {
    let Some(profile_id) = profile_id else {
        return ThirdPartyOAuthProjection {
            applied: false,
            message:
                "未绑定 OAuth 登录档案，ChatGPT.app 将保持当前登录账号；模型请求仍走第三方提供商。"
                    .to_owned(),
        };
    };
    let Some(home) = home else {
        return ThirdPartyOAuthProjection {
            applied: false,
            message: "所选 OAuth 登录档案未能应用；ChatGPT.app 将保持当前登录账号。模型请求仍走第三方提供商。"
                .to_owned(),
        };
    };
    let auth_json = match codex_oauth_profile_auth_json_for_id(
        repository,
        oauth_credentials,
        Some(profile_id),
        CredentialAccess::UserInitiated,
    )
    .await
    {
        Ok(Some(auth_json)) => auth_json,
        Err(AppError::ProfileRuntimeUnavailable) => {
            let _ = profiles::mark_profile_validation_invalid(
                repository,
                profile_id,
                "OAuth 登录凭据已失效，请重新授权后再次应用。",
            );
            return ThirdPartyOAuthProjection {
                applied: false,
                message: "所选 OAuth 登录档案已失效，需重新授权；ChatGPT.app 将保持当前登录账号。模型请求仍走第三方提供商。"
                    .to_owned(),
            };
        }
        Ok(None) | Err(_) => {
            return ThirdPartyOAuthProjection {
                applied: false,
                message: "所选 OAuth 登录档案未能应用；ChatGPT.app 将保持当前登录账号。模型请求仍走第三方提供商。"
                    .to_owned(),
            };
        }
    };
    match write_auth_json_to_home(home, &auth_json) {
        Ok(()) => ThirdPartyOAuthProjection {
            applied: true,
            message: "OAuth 登录档案已写入；ChatGPT.app 重启后将登录所选 OAuth 账号。模型请求仍走第三方提供商。"
                .to_owned(),
        },
        Err(_) => ThirdPartyOAuthProjection {
            applied: false,
            message: "所选 OAuth 登录档案未能应用；ChatGPT.app 将保持当前登录账号。模型请求仍走第三方提供商。"
                .to_owned(),
        },
    }
}

fn project_auth_json_to_home_with_verifier(
    home: &Path,
    auth_json: &str,
    project_desktop: bool,
    verifier: &dyn ProjectedOAuthIdentityVerifier,
) -> AppResult<String> {
    write_auth_json_to_home(home, auth_json)?;
    if project_desktop {
        project_desktop_auth_json(home, auth_json)?;
    }
    let expected_account_id =
        auth_json_account_id(auth_json)?.ok_or(AppError::ProfileRuntimeUnavailable)?;
    verifier.verify(home, &expected_account_id)?;
    Ok("OAuth 登录档案已写入并验证为所选账号；模型请求仍走 Relay 网关。".to_owned())
}

fn auth_json_account_id(auth_json: &str) -> AppResult<Option<String>> {
    let value: serde_json::Value =
        serde_json::from_str(auth_json).map_err(|_| AppError::ValidationFailed)?;
    Ok(value
        .get("tokens")
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned))
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
    match fs::rename(&temporary, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = fs::remove_file(temporary);
            Err(AppError::RuntimeUnavailable)
        }
    }
}

fn project_desktop_auth_json(codex_home: &Path, auth_json: &str) -> AppResult<()> {
    let _ = (codex_home, auth_json);
    Ok(())
}

fn read_codex_account_id_from_app_server(home: &Path) -> AppResult<Option<String>> {
    let mut command = Command::new("codex");
    command
        .args([
            "app-server",
            "--stdio",
            "-c",
            "cli_auth_credentials_store=\"file\"",
        ])
        .env("CODEX_HOME", home)
        .env_remove("CODEX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(codex_spawn_error)?;
    let result = (|| -> AppResult<Option<String>> {
        let mut stdin = child.stdin.take().ok_or(AppError::RuntimeUnavailable)?;
        for request in [
            json!({
                "method": "initialize",
                "id": 1,
                "params": {
                    "clientInfo": {
                        "name": "codex-relay",
                        "title": "Codex Relay",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {}
                }
            }),
            json!({"method": "initialized", "params": {}}),
            json!({"method": "account/read", "id": 2, "params": {"refreshToken": true}}),
        ] {
            serde_json::to_writer(&mut stdin, &request)
                .map_err(|_| AppError::RuntimeUnavailable)?;
            stdin
                .write_all(b"\n")
                .map_err(|_| AppError::RuntimeUnavailable)?;
        }
        stdin.flush().map_err(|_| AppError::RuntimeUnavailable)?;

        let stdout = child.stdout.take().ok_or(AppError::RuntimeUnavailable)?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + CODEX_APP_SERVER_ACCOUNT_READ_TIMEOUT;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let line = receiver
                .recv_timeout(remaining)
                .map_err(|_| AppError::RuntimeUnavailable)?;
            let response: serde_json::Value =
                serde_json::from_str(&line).map_err(|_| AppError::RuntimeUnavailable)?;
            if response.get("id") != Some(&json!(2)) {
                continue;
            }
            if response.get("error").is_some() {
                return Err(app_server_account_error(&response));
            }
            return Ok(response
                .get("result")
                .and_then(|result| result.get("account"))
                .filter(|account| !account.is_null())
                .and_then(|account| account.get("accountId").or_else(|| account.get("id")))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned));
        }
        Err(AppError::RuntimeUnavailable)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn codex_spawn_error(error: std::io::Error) -> AppError {
    if error.kind() == ErrorKind::NotFound {
        AppError::CodexCliMissing
    } else {
        AppError::RuntimeUnavailable
    }
}

fn app_server_account_error(response: &serde_json::Value) -> AppError {
    let detail = response
        .get("error")
        .map(|value| value.to_string())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if detail.contains("401")
        || detail.contains("403")
        || detail.contains("unauthorized")
        || detail.contains("forbidden")
        || detail.contains("auth")
        || detail.contains("login")
    {
        AppError::ProfileRuntimeUnavailable
    } else {
        AppError::RuntimeUnavailable
    }
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
    gateway: Option<GatewayStatus>,
) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    enable_api_profile_at_path(
        repository,
        secrets,
        oauth_credentials.as_ref(),
        id,
        data_dir,
        gateway.as_ref(),
        &path,
    )
    .await
}

async fn enable_api_profile_at_path(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: &OAuthCredentialStore,
    id: &str,
    data_dir: &Path,
    gateway: Option<&GatewayStatus>,
    path: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    let catalog_path = relay_model_catalog_path(path)?;
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
    let config_snapshot = snapshot_managed_config_at_path(secrets.clone(), path).await?;
    let previous_direct_setting = repository.setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)?;
    let result: AppResult<GatewayCodexConfigStatus> = async {
        let projection = project_third_party_oauth_profile_to_home(
            repository,
            oauth_credentials,
            stored.profile.codex_oauth_profile_id.as_deref(),
            path.parent(),
        )
        .await;
        let bridge_service_url = if projection.applied {
            let gateway = gateway
                .filter(|gateway| gateway.running && gateway.certificate_ready)
                .ok_or(AppError::GatewayNotRunning)?;
            Some(format!("{}/v1", gateway.service_url.trim_end_matches('/')))
        } else {
            None
        };
        let original = fs::read_to_string(path).unwrap_or_default();
        let mut document = original
            .parse::<DocumentMut>()
            .map_err(|_| AppError::ValidationFailed)?;
        ensure_relay_file_auth_storage(&mut document);
        let service_url = Some(base_url.clone());
        let message = if bridge_service_url.is_some() {
            format!(
                "Codex 已切换到第三方模型提供商“{}”；OAuth 已应用，模型请求通过 codex_relay_direct 经本机 Relay 固定转发到该提供商。",
                stored.profile.alias
            )
        } else {
            format!(
                "Codex 已切换到第三方模型提供商“{}”直连。 {}",
                stored.profile.alias, projection.message
            )
        };
        document["model_provider"] = value(CODEX_RELAY_DIRECT_PROVIDER);
        document["model"] = value(model);
        document["model_catalog_json"] = value(catalog_path.display().to_string());
        let providers = document["model_providers"].or_insert(Item::Table(Table::new()));
        let providers = providers
            .as_table_like_mut()
            .ok_or(AppError::ValidationFailed)?;
        let mut provider = Table::new();
        provider["name"] = value(format!("Codex Relay · {}", stored.profile.alias));
        provider["base_url"] = value(
            bridge_service_url
                .clone()
                .unwrap_or_else(|| base_url.clone()),
        );
        provider["wire_api"] = value("responses");
        if bridge_service_url.is_some() {
            provider["requires_openai_auth"] = value(true);
        } else {
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
        }
        providers.insert(CODEX_RELAY_DIRECT_PROVIDER, Item::Table(provider));
        repair_managed_provider_auth_conflicts(&mut document);
        let generated = document.to_string();
        store_config_snapshot(secrets.clone(), &original, &generated).await?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
            write_direct_model_catalog(&catalog_path, &mappings)?;
        }
        fs::write(path, generated).map_err(|_| AppError::RuntimeUnavailable)?;
        repository.set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, &stored.profile.id)?;
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
    .await;

    match result {
        Ok(status) => Ok(status),
        Err(error) => {
            let _ = restore_managed_config(secrets, config_snapshot).await;
            restore_setting(
                repository,
                GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING,
                previous_direct_setting,
            );
            Err(error)
        }
    }
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
            {"effort": "minimal", "description": "Minimal reasoning"},
            {"effort": "low", "description": "Low reasoning"},
            {"effort": "medium", "description": "Medium reasoning"},
            {"effort": "high", "description": "High reasoning"},
            {"effort": "xhigh", "description": "Extra high reasoning"}
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
    let mut result = status(repository, secrets.clone()).await?;
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
        if let Some(original_auth_storage) = original.get(CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY) {
            current[CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY] = original_auth_storage.clone();
        } else {
            current
                .as_table_mut()
                .remove(CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY);
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
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };
    use async_trait::async_trait;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use toml_edit::DocumentMut;
    use uuid::Uuid;

    use super::{
        codex_oauth_profile_options, content_hash, enable_api_profile_at_path, enable_for_path,
        gateway_model_options, project_third_party_oauth_profile_to_home, relay_auth_config,
        remove_relay_config, repair_managed_config_at_path, repair_managed_provider_auth_conflicts,
        set_codex_oauth_profile_at_path, set_codex_oauth_profile_at_path_with_verifier,
        snapshot_managed_config_at_path, status_for_path, sync_active_direct_oauth_profile_at_path,
        timestamp_ms, AuthJsonProjectedOAuthIdentityVerifier, CodexOAuthProfileSwitchContext,
        ConfigSnapshot, MismatchedProjectedOAuthIdentityVerifier,
        CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE, CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY,
        CODEX_RELAY_MODEL_CATALOG_FILENAME, SNAPSHOT_REF,
    };
    use crate::{
        database::{Repository, StoredProfile},
        domain::{
            CodexAuthMode, GatewayModelMapping, GatewayNetworkAddress, GatewayProvider,
            GatewayStatus, GatewayWireApi, MaskedClientKey, MaskedProfile, ProfileKind,
            SetCodexGatewayOAuthProfileInput, GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
            GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
        },
        error::AppError,
        oauth_credentials::OAuthCredentialStore,
        profiles::CodexOAuthCredential,
        secrets::{LocalEncryptedSecretStore, MemorySecretStore, SecretStore},
    };

    fn jwt_with_expiry(expiry_ms: i64) -> String {
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&serde_json::json!({"exp": expiry_ms / 1000})).unwrap());
        format!("header.{payload}.signature")
    }

    #[test]
    fn detects_config_changes_before_restore() {
        assert_ne!(
            content_hash("model_provider = 'one'"),
            content_hash("model_provider = 'two'")
        );
    }

    #[tokio::test]
    async fn managed_config_snapshot_restores_config_catalog_auth_and_secret_snapshot() {
        let root = temp_root("codex-gateway-managed-snapshot");
        let config_path = root.join("config.toml");
        let catalog_path = root.join(CODEX_RELAY_MODEL_CATALOG_FILENAME);
        let auth_path = root.join("auth.json");
        std::fs::write(&config_path, "model_provider = 'codex_relay'\n").unwrap();
        std::fs::write(&catalog_path, r#"{"models":["before"]}"#).unwrap();
        std::fs::write(&auth_path, r#"{"tokens":{"access_token":"before"}}"#).unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        secrets.set(SNAPSHOT_REF, "before-snapshot").await.unwrap();
        let snapshot = snapshot_managed_config_at_path(secrets.clone(), &config_path)
            .await
            .unwrap();

        std::fs::write(&config_path, "model_provider = 'changed'\n").unwrap();
        std::fs::remove_file(&catalog_path).unwrap();
        std::fs::write(&auth_path, r#"{"tokens":{"access_token":"changed"}}"#).unwrap();
        secrets.set(SNAPSHOT_REF, "changed-snapshot").await.unwrap();

        super::restore_managed_config(secrets.clone(), snapshot)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "model_provider = 'codex_relay'\n"
        );
        assert_eq!(
            std::fs::read_to_string(&catalog_path).unwrap(),
            r#"{"models":["before"]}"#
        );
        assert_eq!(
            std::fs::read_to_string(&auth_path).unwrap(),
            r#"{"tokens":{"access_token":"before"}}"#
        );
        assert_eq!(secrets.get(SNAPSHOT_REF).await.unwrap(), "before-snapshot");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn repairs_only_managed_provider_auth_conflicts() {
        let mut document = r#"
model_provider = "codex_relay_direct"

[model_providers.codex_relay]
requires_openai_auth = true

[model_providers.codex_relay.auth]
command = "/tmp/relay"

[model_providers.codex_relay_direct]
requires_openai_auth = false

[model_providers.codex_relay_direct.auth]
command = "/tmp/direct"

[model_providers.managed_without_auth]
requires_openai_auth = true

[model_providers.custom]
requires_openai_auth = true

[model_providers.custom.auth]
command = "/tmp/custom"
"#
        .parse::<DocumentMut>()
        .unwrap();

        assert!(repair_managed_provider_auth_conflicts(&mut document));
        assert!(document["model_providers"]["codex_relay"]
            .get("requires_openai_auth")
            .is_none());
        assert!(document["model_providers"]["codex_relay_direct"]
            .get("requires_openai_auth")
            .is_none());
        assert_eq!(
            document["model_providers"]["custom"]["requires_openai_auth"].as_bool(),
            Some(true)
        );
        assert!(!repair_managed_provider_auth_conflicts(&mut document));
    }

    #[tokio::test]
    async fn managed_config_repair_rebases_only_an_owned_snapshot() {
        let root = temp_root("codex-gateway-managed-repair-snapshot");
        let config_path = root.join("config.toml");
        let catalog_path = root.join(CODEX_RELAY_MODEL_CATALOG_FILENAME);
        std::fs::write(&catalog_path, r#"{"models":[]}"#).unwrap();
        let original = format!(
            r#"model_provider = "codex_relay"
model_catalog_json = "{CODEX_RELAY_MODEL_CATALOG_FILENAME}"

[model_providers.codex_relay]
requires_openai_auth = true

[model_providers.codex_relay.auth]
command = "/tmp/relay"
"#
        );
        std::fs::write(&config_path, &original).unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let snapshot = ConfigSnapshot {
            original: "model_provider = 'openai'\n".to_owned(),
            generated_hash: content_hash(&original),
        };
        secrets
            .set(SNAPSHOT_REF, &serde_json::to_string(&snapshot).unwrap())
            .await
            .unwrap();

        assert!(
            repair_managed_config_at_path(Some(secrets.clone()), &config_path)
                .await
                .unwrap()
        );
        let repaired = std::fs::read_to_string(&config_path).unwrap();
        let document = repaired.parse::<DocumentMut>().unwrap();
        assert!(document["model_providers"]["codex_relay"]
            .get("requires_openai_auth")
            .is_none());
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some(catalog_path.display().to_string().as_str())
        );
        assert_eq!(
            document[CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY].as_str(),
            Some(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE)
        );
        let rebased: ConfigSnapshot =
            serde_json::from_str(&secrets.get(SNAPSHOT_REF).await.unwrap()).unwrap();
        assert_eq!(rebased.original, snapshot.original);
        assert_eq!(rebased.generated_hash, content_hash(&repaired));
        assert!(!repair_managed_config_at_path(Some(secrets), &config_path)
            .await
            .unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn managed_config_repair_preserves_unowned_snapshot() {
        let root = temp_root("codex-gateway-unowned-repair-snapshot");
        let config_path = root.join("config.toml");
        let original = r#"model_provider = "codex_relay_direct"

[model_providers.codex_relay_direct]
requires_openai_auth = true

[model_providers.codex_relay_direct.auth]
command = "/tmp/direct"
"#;
        std::fs::write(&config_path, original).unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let stored = serde_json::to_string(&ConfigSnapshot {
            original: "model_provider = 'openai'\n".to_owned(),
            generated_hash: content_hash("externally changed"),
        })
        .unwrap();
        secrets.set(SNAPSHOT_REF, &stored).await.unwrap();

        assert!(
            repair_managed_config_at_path(Some(secrets.clone()), &config_path)
                .await
                .unwrap()
        );
        assert_eq!(secrets.get(SNAPSHOT_REF).await.unwrap(), stored);
        let _ = std::fs::remove_dir_all(root);
    }

    struct FailingSnapshotSecretStore {
        values: std::sync::Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for FailingSnapshotSecretStore {
        async fn set(&self, reference: &str, value: &str) -> crate::error::AppResult<()> {
            if reference == SNAPSHOT_REF {
                return Err(AppError::SecretStoreUnavailable);
            }
            self.values
                .lock()
                .map_err(|_| AppError::Internal)?
                .insert(reference.to_owned(), value.to_owned());
            Ok(())
        }

        async fn get(&self, reference: &str) -> crate::error::AppResult<String> {
            self.values
                .lock()
                .map_err(|_| AppError::Internal)?
                .get(reference)
                .cloned()
                .ok_or(AppError::NotFound)
        }

        async fn delete(&self, reference: &str) -> crate::error::AppResult<()> {
            self.values
                .lock()
                .map_err(|_| AppError::Internal)?
                .remove(reference);
            Ok(())
        }
    }

    #[tokio::test]
    async fn managed_config_repair_restores_file_when_snapshot_rebase_fails() {
        let root = temp_root("codex-gateway-repair-snapshot-failure");
        let config_path = root.join("config.toml");
        let original = r#"model_provider = "codex_relay"

[model_providers.codex_relay]
requires_openai_auth = true

[model_providers.codex_relay.auth]
command = "/tmp/relay"
"#;
        std::fs::write(&config_path, original).unwrap();
        let snapshot = serde_json::to_string(&ConfigSnapshot {
            original: "model_provider = 'openai'\n".to_owned(),
            generated_hash: content_hash(original),
        })
        .unwrap();
        let secrets = Arc::new(FailingSnapshotSecretStore {
            values: std::sync::Mutex::new(HashMap::from([(
                SNAPSHOT_REF.to_owned(),
                snapshot.clone(),
            )])),
        });

        let error = repair_managed_config_at_path(Some(secrets.clone()), &config_path)
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::SecretStoreUnavailable));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert_eq!(secrets.get(SNAPSHOT_REF).await.unwrap(), snapshot);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removes_only_relay_settings_when_config_changed_externally() {
        let original = "model_provider = 'openai'\nmodel = 'gpt-5'\nmodel_catalog_json = 'official-catalog.json'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut current = "model_provider = 'codex_relay_direct'\nmodel = 'provider-real'\nmodel_catalog_json = 'codex-relay-model-catalog.json'\ncli_auth_credentials_store = 'file'\n[model_providers.codex_relay]\nbase_url = 'https://relay'\n[model_providers.codex_relay_direct]\nbase_url = 'https://api.example.com/v1'\n[custom]\nvalue = 'keep'\n"
            .parse::<DocumentMut>()
            .unwrap();
        remove_relay_config(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("openai"));
        assert_eq!(current["model"].as_str(), Some("gpt-5"));
        assert!(current.get("model_catalog_json").is_none());
        assert!(current.get(CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY).is_none());
        assert!(current["model_providers"].get("codex_relay").is_none());
        assert!(current["model_providers"]
            .get("codex_relay_direct")
            .is_none());
        assert_eq!(current["custom"]["value"].as_str(), Some("keep"));
    }

    #[test]
    fn removes_relay_owned_absolute_model_catalog_when_restoring_config() {
        let original = "model_provider = 'openai'\nmodel = 'gpt-5'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let catalog_path =
            std::env::temp_dir().join(format!("relay/{CODEX_RELAY_MODEL_CATALOG_FILENAME}"));
        let mut current = format!(
            "model_provider = 'codex_relay'\nmodel_catalog_json = '{}'\n",
            catalog_path.display()
        )
        .parse::<DocumentMut>()
        .unwrap();

        remove_relay_config(&mut current, &original);

        assert!(current.get("model_catalog_json").is_none());
    }

    #[tokio::test]
    async fn managed_config_repair_migrates_relay_owned_model_catalog_path() {
        let root = temp_root("codex-gateway-catalog-migration");
        let config_path = root.join("config.toml");
        let catalog_path = root.join(CODEX_RELAY_MODEL_CATALOG_FILENAME);
        std::fs::write(&catalog_path, r#"{"models":[]}"#).unwrap();
        std::fs::write(
            &config_path,
            format!(
                "model_provider = 'codex_relay'\nmodel_catalog_json = '{CODEX_RELAY_MODEL_CATALOG_FILENAME}'\n"
            ),
        )
        .unwrap();

        repair_managed_config_at_path(None, &config_path)
            .await
            .unwrap();

        let document = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let expected = catalog_path.display().to_string();
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some(expected.as_str())
        );
        assert_eq!(
            document[CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY].as_str(),
            Some(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn managed_config_repair_leaves_official_auth_storage_untouched() {
        let root = temp_root("codex-gateway-official-auth-storage");
        let config_path = root.join("config.toml");
        let original = "model_provider = 'openai'\ncli_auth_credentials_store = 'keyring'\n";
        std::fs::write(&config_path, original).unwrap();

        assert!(!repair_managed_config_at_path(None, &config_path)
            .await
            .unwrap());

        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_does_not_migrate_an_official_relative_model_catalog() {
        let repository = Repository::memory();
        let root = temp_root("codex-gateway-official-catalog");
        let config_path = root.join("config.toml");
        let original =
            "model_provider = 'openai'\nmodel_catalog_json = 'codex-relay-model-catalog.json'\n";
        std::fs::write(&config_path, original).unwrap();

        let status = status_for_path(&repository, &config_path).unwrap();

        assert!(!status.enabled);
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_marks_a_missing_relay_model_catalog_for_repair() {
        let repository = Repository::memory();
        let root = temp_root("codex-gateway-missing-catalog");
        let config_path = root.join("config.toml");
        let catalog_path = root.join(CODEX_RELAY_MODEL_CATALOG_FILENAME);
        std::fs::write(
            &config_path,
            format!(
                "model_provider = 'codex_relay_direct'\nmodel_catalog_json = '{}'\n",
                catalog_path.display()
            ),
        )
        .unwrap();

        let status = status_for_path(&repository, &config_path).unwrap();

        assert!(status.enabled);
        assert!(status.needs_repair);
        assert!(status.message.contains("模型目录文件不存在"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_marks_relay_auth_storage_for_repair() {
        let repository = Repository::memory();
        let root = temp_root("codex-gateway-auth-storage-repair");
        let config_path = root.join("config.toml");
        let catalog_path = root.join(CODEX_RELAY_MODEL_CATALOG_FILENAME);
        std::fs::write(&catalog_path, r#"{"models":[]}"#).unwrap();
        std::fs::write(
            &config_path,
            format!(
                "model_provider = 'codex_relay'\nmodel_catalog_json = '{}'\ncli_auth_credentials_store = 'keyring'\n",
                catalog_path.display()
            ),
        )
        .unwrap();

        let status = status_for_path(&repository, &config_path).unwrap();

        assert!(status.enabled);
        assert!(status.needs_repair);
        assert!(status.message.contains("auth.json 文件"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn codex_oauth_profile_setting_round_trips_into_status() {
        let repository = Repository::memory();
        insert_oauth_profile(&repository, "oauth", "插件账号");
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        let root = temp_root("codex-oauth-setting");
        let config_path = root.join("config.toml");

        let status = set_codex_oauth_profile_at_path(
            &repository,
            secrets.clone(),
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("oauth".to_owned()),
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: None,
                path: &config_path,
                project_desktop: false,
            },
        )
        .await
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

        let status = set_codex_oauth_profile_at_path(
            &repository,
            secrets,
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: None,
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: None,
                path: &config_path,
                project_desktop: false,
            },
        )
        .await
        .unwrap();
        assert!(repository
            .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
            .unwrap()
            .is_none());
        assert!(status.oauth_profile_id.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn relay_oauth_selection_removes_legacy_auth_conflict_and_projects_identity() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-b", "账号 B");
        let credential = CodexOAuthCredential {
            id_token: "id-oauth-b".to_owned(),
            access_token: "access-b".to_owned(),
            refresh_token: Some("refresh-b".to_owned()),
            account_id: Some("account-b".to_owned()),
            last_refresh_ms: 1,
        };
        secrets
            .set(
                "profile:oauth-b:oauth",
                &serde_json::to_string(&credential).unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-relay-oauth-selection");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
model_provider = "codex_relay"
model = "provider-real"

[model_providers.codex_relay]
name = "Codex Relay LAN Gateway"
base_url = "http://127.0.0.1:53111/v1"
wire_api = "responses"
requires_openai_auth = true

[model_providers.codex_relay.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "client-key:test", "--relay-data-dir", "/tmp/data"]
"#,
        )
        .unwrap();
        let verifier = AuthJsonProjectedOAuthIdentityVerifier;

        let status = set_codex_oauth_profile_at_path_with_verifier(
            &repository,
            secrets.clone(),
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("oauth-b".to_owned()),
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: None,
                path: &config_path,
                project_desktop: false,
            },
            &verifier,
        )
        .await
        .unwrap();

        assert_eq!(status.mode, "relay_gateway");
        assert_eq!(status.oauth_profile_id.as_deref(), Some("oauth-b"));
        assert!(status.message.contains("模型请求仍走 Relay 网关"));
        let auth: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(auth["tokens"]["account_id"], "account-b");
        assert_eq!(auth["tokens"]["access_token"], "access-b");
        assert_eq!(auth["tokens"]["refresh_token"], "refresh-b");
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(config["model_provider"].as_str(), Some("codex_relay"));
        assert_eq!(
            config["model_providers"]["codex_relay"]["base_url"].as_str(),
            Some("http://127.0.0.1:53111/v1")
        );
        assert!(config["model_providers"]["codex_relay"]
            .get("requires_openai_auth")
            .is_none());
        assert!(config["model_providers"]["codex_relay"]
            .get("auth")
            .is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn relay_oauth_selection_still_rolls_back_account_mismatch() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-b", "账号 B");
        secrets
            .set(
                "profile:oauth-b:oauth",
                &serde_json::to_string(&CodexOAuthCredential {
                    id_token: "id-oauth-b".to_owned(),
                    access_token: "access-b".to_owned(),
                    refresh_token: Some("refresh-b".to_owned()),
                    account_id: Some("account-b".to_owned()),
                    last_refresh_ms: 1,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-relay-oauth-mismatch");
        let config_path = root.join("config.toml");
        let auth_path = root.join("auth.json");
        let original_config = r#"
model_provider = "codex_relay"
model = "provider-real"

[model_providers.codex_relay]
name = "Codex Relay LAN Gateway"
base_url = "http://127.0.0.1:53111/v1"
wire_api = "responses"

[model_providers.codex_relay.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "client-key:test", "--relay-data-dir", "/tmp/data"]
"#;
        let original_auth = r#"{"tokens":{"account_id":"current-account"}}"#;
        std::fs::write(&config_path, original_config).unwrap();
        std::fs::write(&auth_path, original_auth).unwrap();
        let verifier = MismatchedProjectedOAuthIdentityVerifier;

        let result = set_codex_oauth_profile_at_path_with_verifier(
            &repository,
            secrets.clone(),
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("oauth-b".to_owned()),
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: None,
                path: &config_path,
                project_desktop: false,
            },
            &verifier,
        )
        .await;

        assert!(matches!(result, Err(AppError::OAuthIdentityMismatch)));
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            original_config
        );
        assert_eq!(std::fs::read_to_string(auth_path).unwrap(), original_auth);
        assert!(repository
            .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn direct_oauth_selection_projects_selected_identity_without_changing_provider() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-a", "账号 A");
        insert_oauth_profile(&repository, "oauth-b", "账号 B");
        insert_api_profile(
            &repository,
            "api-direct",
            "Third Party",
            "healthy",
            None,
            vec!["provider-real"],
            Vec::new(),
        );
        let mut direct = repository.profile("api-direct").unwrap();
        direct.profile.codex_oauth_profile_id = Some("oauth-a".to_owned());
        repository.update_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "api-direct")
            .unwrap();
        repository
            .set_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING, "oauth-a")
            .unwrap();
        for (id, account, access, refresh) in [
            ("oauth-a", "account-a", "access-a", "refresh-a"),
            ("oauth-b", "account-b", "access-b", "refresh-b"),
        ] {
            let credential = CodexOAuthCredential {
                id_token: format!("id-{id}"),
                access_token: access.to_owned(),
                refresh_token: Some(refresh.to_owned()),
                account_id: Some(account.to_owned()),
                last_refresh_ms: 1,
            };
            secrets
                .set(
                    &format!("profile:{id}:oauth"),
                    &serde_json::to_string(&credential).unwrap(),
                )
                .await
                .unwrap();
        }
        let root = temp_root("codex-direct-oauth-selection");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
model_provider = "codex_relay_direct"
model = "provider-real"

[model_providers.codex_relay_direct]
name = "Third Party"
base_url = "https://api.example.com/v1"
wire_api = "responses"
requires_openai_auth = true

[model_providers.codex_relay_direct.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "profile:api-direct:credential", "--relay-data-dir", "/tmp/data"]
"#,
        )
        .unwrap();

        let verifier = AuthJsonProjectedOAuthIdentityVerifier;
        let gateway = running_gateway();
        let status = set_codex_oauth_profile_at_path_with_verifier(
            &repository,
            secrets.clone(),
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("oauth-b".to_owned()),
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: Some(&gateway),
                path: &config_path,
                project_desktop: false,
            },
            &verifier,
        )
        .await
        .unwrap();

        assert_eq!(status.mode, "third_party");
        assert_eq!(status.oauth_profile_id.as_deref(), Some("oauth-b"));
        assert_eq!(
            repository
                .profile("api-direct")
                .unwrap()
                .profile
                .codex_oauth_profile_id
                .as_deref(),
            Some("oauth-b")
        );
        assert_eq!(
            repository
                .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
                .unwrap()
                .as_deref(),
            Some("oauth-a")
        );
        let auth: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(auth["tokens"]["account_id"], "account-b");
        assert_eq!(auth["tokens"]["access_token"], "access-b");
        assert_eq!(auth["tokens"]["refresh_token"], "refresh-b");
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            config["model_provider"].as_str(),
            Some("codex_relay_direct")
        );
        assert_eq!(config["model"].as_str(), Some("provider-real"));
        assert_eq!(
            config["model_providers"]["codex_relay_direct"]["base_url"].as_str(),
            Some("https://10.12.14.248:53765/v1")
        );
        assert_eq!(
            config["model_providers"]["codex_relay_direct"]["requires_openai_auth"].as_bool(),
            Some(true)
        );
        assert!(config["model_providers"]["codex_relay_direct"]
            .get("auth")
            .is_none());
        assert_eq!(
            status.service_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn direct_oauth_selection_ignores_previous_account_mismatch() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-b", "账号 B");
        insert_api_profile(
            &repository,
            "api-direct",
            "Third Party",
            "healthy",
            None,
            vec!["provider-real"],
            Vec::new(),
        );
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "api-direct")
            .unwrap();
        let credential = CodexOAuthCredential {
            id_token: "id-oauth-b".to_owned(),
            access_token: "access-b".to_owned(),
            refresh_token: Some("refresh-b".to_owned()),
            account_id: Some("account-b".to_owned()),
            last_refresh_ms: 1,
        };
        secrets
            .set(
                "profile:oauth-b:oauth",
                &serde_json::to_string(&credential).unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-direct-oauth-mismatch");
        let config_path = root.join("config.toml");
        let original_config = r#"
model_provider = "codex_relay_direct"
model = "provider-real"

[model_providers.codex_relay_direct]
name = "Third Party"
base_url = "https://api.example.com/v1"
wire_api = "responses"
requires_openai_auth = true

[model_providers.codex_relay_direct.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "profile:api-direct:credential", "--relay-data-dir", "/tmp/data"]
"#;
        std::fs::write(&config_path, original_config).unwrap();
        let verifier = MismatchedProjectedOAuthIdentityVerifier;
        let gateway = running_gateway();

        let status = set_codex_oauth_profile_at_path_with_verifier(
            &repository,
            secrets.clone(),
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("oauth-b".to_owned()),
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: Some(&gateway),
                path: &config_path,
                project_desktop: false,
            },
            &verifier,
        )
        .await
        .unwrap();

        assert_eq!(status.mode, "third_party");
        assert!(status.message.contains("OAuth 已应用"));
        assert!(status.message.contains("经本机 Relay 固定转发"));
        let auth: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(auth["tokens"]["account_id"], "account-b");
        assert_eq!(
            repository
                .profile("api-direct")
                .unwrap()
                .profile
                .codex_oauth_profile_id
                .as_deref(),
            Some("oauth-b")
        );
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            config["model_provider"].as_str(),
            Some("codex_relay_direct")
        );
        assert_eq!(config["model"].as_str(), Some("provider-real"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn api_profile_activation_ignores_previous_account_mismatch() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        insert_oauth_profile(&repository, "oauth-login", "登录账号");
        insert_api_profile(
            &repository,
            "api-direct",
            "Third Party",
            "healthy",
            None,
            vec!["provider-real"],
            Vec::new(),
        );
        let mut direct = repository.profile("api-direct").unwrap();
        direct.profile.codex_oauth_profile_id = Some("oauth-login".to_owned());
        repository.update_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "previous-direct")
            .unwrap();
        secrets
            .set("profile:api-direct:credential", "sk-test")
            .await
            .unwrap();
        secrets
            .set(
                "profile:oauth-login:oauth",
                &serde_json::to_string(&CodexOAuthCredential {
                    id_token: "id-oauth-login".to_owned(),
                    access_token: "access-login".to_owned(),
                    refresh_token: Some("refresh-login".to_owned()),
                    account_id: Some("account-login".to_owned()),
                    last_refresh_ms: 1,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-api-profile-mismatch-rollback");
        let data_dir = root.join("data");
        let config_path = root.join("home/.codex/config.toml");
        let catalog_path = root.join("home/.codex/codex-relay-model-catalog.json");
        let auth_path = root.join("home/.codex/auth.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original_config = "model_provider = 'openai'\nmodel = 'gpt-5'\n";
        let original_catalog = r#"{"models":["before"]}"#;
        let original_auth = r#"{"tokens":{"account_id":"before"}}"#;
        std::fs::write(&config_path, original_config).unwrap();
        std::fs::write(&catalog_path, original_catalog).unwrap();
        std::fs::write(&auth_path, original_auth).unwrap();
        let gateway = running_gateway();
        let status = enable_api_profile_at_path(
            &repository,
            secrets.clone(),
            oauth_credentials.as_ref(),
            "api-direct",
            &data_dir,
            Some(&gateway),
            &config_path,
        )
        .await
        .unwrap();

        assert_eq!(status.mode, "third_party");
        assert!(status.message.contains("OAuth 已应用"));
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            config["model_provider"].as_str(),
            Some("codex_relay_direct")
        );
        assert_eq!(
            config["model_providers"]["codex_relay_direct"]["base_url"].as_str(),
            Some("https://10.12.14.248:53765/v1")
        );
        assert_eq!(
            config["model_providers"]["codex_relay_direct"]["requires_openai_auth"].as_bool(),
            Some(true)
        );
        assert!(config["model_providers"]["codex_relay_direct"]
            .get("auth")
            .is_none());
        assert_eq!(
            status.service_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert!(std::fs::read_to_string(&catalog_path)
            .unwrap()
            .contains("provider-real"));
        let auth: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(auth["tokens"]["account_id"], "account-login");
        assert_eq!(
            repository
                .setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)
                .unwrap()
                .as_deref(),
            Some("api-direct")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oauth_bridge_activation_rolls_back_when_gateway_is_unavailable() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-login", "登录账号");
        insert_api_profile(
            &repository,
            "api-direct",
            "Zeron",
            "healthy",
            None,
            vec!["provider-real"],
            Vec::new(),
        );
        let mut direct = repository.profile("api-direct").unwrap();
        direct.profile.codex_oauth_profile_id = Some("oauth-login".to_owned());
        repository.update_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "previous-direct")
            .unwrap();
        secrets
            .set("profile:api-direct:credential", "zeron-key")
            .await
            .unwrap();
        secrets
            .set(
                "profile:oauth-login:oauth",
                &serde_json::to_string(&CodexOAuthCredential {
                    id_token: "id-oauth-login".to_owned(),
                    access_token: "access-login".to_owned(),
                    refresh_token: Some("refresh-login".to_owned()),
                    account_id: Some("account-login".to_owned()),
                    last_refresh_ms: 1,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-api-bridge-gateway-rollback");
        let data_dir = root.join("data");
        let config_path = root.join("home/.codex/config.toml");
        let catalog_path = root.join("home/.codex/codex-relay-model-catalog.json");
        let auth_path = root.join("home/.codex/auth.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original_config = "model_provider = 'openai'\nmodel = 'gpt-5'\n";
        let original_catalog = r#"{"models":["before"]}"#;
        let original_auth = r#"{"tokens":{"access_token":"before","account_id":"before"}}"#;
        std::fs::write(&config_path, original_config).unwrap();
        std::fs::write(&catalog_path, original_catalog).unwrap();
        std::fs::write(&auth_path, original_auth).unwrap();

        let error = enable_api_profile_at_path(
            &repository,
            secrets.clone(),
            &oauth_credentials,
            "api-direct",
            &data_dir,
            None,
            &config_path,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AppError::GatewayNotRunning));
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            original_config
        );
        assert_eq!(
            std::fs::read_to_string(&catalog_path).unwrap(),
            original_catalog
        );
        assert_eq!(std::fs::read_to_string(&auth_path).unwrap(), original_auth);
        assert_eq!(
            repository
                .setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)
                .unwrap()
                .as_deref(),
            Some("previous-direct")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn third_party_oauth_write_failure_returns_a_nonblocking_message() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-login", "登录账号");
        secrets
            .set(
                "profile:oauth-login:oauth",
                &serde_json::to_string(&CodexOAuthCredential {
                    id_token: "id-oauth-login".to_owned(),
                    access_token: "access-login".to_owned(),
                    refresh_token: Some("refresh-login".to_owned()),
                    account_id: Some("account-login".to_owned()),
                    last_refresh_ms: 1,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-third-party-oauth-write-failure");
        let auth_path = root.join("auth.json");
        std::fs::create_dir_all(&auth_path).unwrap();

        let message = project_third_party_oauth_profile_to_home(
            &repository,
            &oauth_credentials,
            Some("oauth-login"),
            Some(&root),
        )
        .await;

        assert!(message.message.contains("所选 OAuth 登录档案未能应用"));
        assert!(message.message.contains("模型请求仍走第三方提供商"));
        assert!(auth_path.is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn active_direct_profile_update_projects_selected_oauth_identity_only() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-a", "账号 A");
        insert_oauth_profile(&repository, "oauth-b", "账号 B");
        insert_api_profile(
            &repository,
            "api-direct",
            "Third Party",
            "healthy",
            None,
            vec!["provider-real"],
            Vec::new(),
        );
        let mut direct = repository.profile("api-direct").unwrap();
        direct.profile.codex_oauth_profile_id = Some("oauth-b".to_owned());
        repository.update_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "api-direct")
            .unwrap();
        repository
            .set_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING, "oauth-a")
            .unwrap();
        for (id, account, access, refresh) in [
            ("oauth-a", "account-a", "access-a", "refresh-a"),
            ("oauth-b", "account-b", "access-b", "refresh-b"),
        ] {
            let credential = CodexOAuthCredential {
                id_token: format!("id-{id}"),
                access_token: access.to_owned(),
                refresh_token: Some(refresh.to_owned()),
                account_id: Some(account.to_owned()),
                last_refresh_ms: 1,
            };
            secrets
                .set(
                    &format!("profile:{id}:oauth"),
                    &serde_json::to_string(&credential).unwrap(),
                )
                .await
                .unwrap();
        }
        let root = temp_root("codex-direct-profile-update-oauth");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
model_provider = "codex_relay_direct"
model = "provider-real"

[model_providers.codex_relay_direct]
name = "Third Party"
base_url = "https://api.example.com/v1"
wire_api = "responses"
requires_openai_auth = true

[model_providers.codex_relay_direct.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "profile:api-direct:credential", "--relay-data-dir", "/tmp/data"]
"#,
        )
        .unwrap();

        let status = sync_active_direct_oauth_profile_at_path(
            &repository,
            &oauth_credentials,
            "api-direct",
            &config_path,
        )
        .await
        .unwrap();

        assert_eq!(status.mode, "third_party");
        assert_eq!(status.oauth_profile_id.as_deref(), Some("oauth-b"));
        assert!(status
            .message
            .contains("OAuth 登录档案已写入；ChatGPT.app 重启后将登录所选 OAuth 账号"));
        let auth: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(auth["tokens"]["account_id"], "account-b");
        assert_eq!(auth["tokens"]["access_token"], "access-b");
        assert_eq!(auth["tokens"]["refresh_token"], "refresh-b");
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            config["model_provider"].as_str(),
            Some("codex_relay_direct")
        );
        assert_eq!(config["model"].as_str(), Some("provider-real"));
        assert_eq!(
            config["model_providers"]["codex_relay_direct"]["base_url"].as_str(),
            Some("https://api.example.com/v1")
        );
        assert!(config["model_providers"]["codex_relay_direct"]
            .get("requires_openai_auth")
            .is_none());
        assert!(config["model_providers"]["codex_relay_direct"]
            .get("auth")
            .is_some());
        assert_eq!(
            repository
                .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
                .unwrap()
                .as_deref(),
            Some("oauth-a")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn active_direct_profile_update_keeps_login_for_invalid_oauth() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets);
        insert_oauth_profile(&repository, "oauth-invalid", "失效账号");
        let mut invalid = repository.profile("oauth-invalid").unwrap();
        invalid.profile.validation_status = "invalid".to_owned();
        invalid.profile.validation_message = Some("OAuth 已失效".to_owned());
        repository.update_profile(&invalid).unwrap();
        insert_api_profile(
            &repository,
            "api-direct",
            "Third Party",
            "healthy",
            None,
            vec!["provider-real"],
            Vec::new(),
        );
        let mut direct = repository.profile("api-direct").unwrap();
        direct.profile.codex_oauth_profile_id = Some("oauth-invalid".to_owned());
        repository.update_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "api-direct")
            .unwrap();
        let root = temp_root("codex-direct-profile-update-invalid-oauth");
        let config_path = root.join("config.toml");
        let auth_path = root.join("auth.json");
        std::fs::write(
            &config_path,
            r#"
model_provider = "codex_relay_direct"
model = "provider-real"

[model_providers.codex_relay_direct]
base_url = "https://api.example.com/v1"
"#,
        )
        .unwrap();
        let original_auth = r#"{"tokens":{"account_id":"current-account"}}"#;
        std::fs::write(&auth_path, original_auth).unwrap();

        let status = sync_active_direct_oauth_profile_at_path(
            &repository,
            &oauth_credentials,
            "api-direct",
            &config_path,
        )
        .await
        .unwrap();

        assert_eq!(status.mode, "third_party");
        assert!(status
            .message
            .contains("所选 OAuth 登录档案已失效，需重新授权"));
        assert!(status.message.contains("模型请求仍走第三方提供商"));
        assert_eq!(std::fs::read_to_string(auth_path).unwrap(), original_auth);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn third_party_projection_keeps_login_when_expired_id_token_cannot_refresh() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        insert_oauth_profile(&repository, "oauth-expired", "过期账号");
        let now = timestamp_ms();
        secrets
            .set(
                "profile:oauth-expired:oauth",
                &serde_json::to_string(&CodexOAuthCredential {
                    id_token: jwt_with_expiry(now - 60_000),
                    access_token: jwt_with_expiry(now + 86_400_000),
                    refresh_token: None,
                    account_id: Some("expired-account".to_owned()),
                    last_refresh_ms: now - 60_000,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let root = temp_root("codex-direct-expired-id-token");
        let auth_path = root.join("auth.json");
        let original_auth = r#"{"tokens":{"account_id":"current-account"}}"#;
        std::fs::write(&auth_path, original_auth).unwrap();

        let message = project_third_party_oauth_profile_to_home(
            &repository,
            &oauth_credentials,
            Some("oauth-expired"),
            Some(&root),
        )
        .await;

        assert!(message
            .message
            .contains("所选 OAuth 登录档案已失效，需重新授权"));
        assert!(message.message.contains("模型请求仍走第三方提供商"));
        assert_eq!(std::fs::read_to_string(auth_path).unwrap(), original_auth);
        let profile = repository.profile("oauth-expired").unwrap().profile;
        assert_eq!(profile.validation_status, "invalid");
        assert_eq!(profile.health, "reauthorization_required");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn codex_oauth_profile_options_mark_unavailable_reasons() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = OAuthCredentialStore::new(secrets.clone());
        let root = temp_root("codex-oauth-options");
        let config_path = root.join("config.toml");
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
        assert!(set_codex_oauth_profile_at_path(
            &repository,
            secrets,
            &oauth_credentials,
            SetCodexGatewayOAuthProfileInput {
                profile_id: Some("json-import".to_owned()),
                confirmed: true,
            },
            CodexOAuthProfileSwitchContext {
                data_dir: &root.join("data"),
                gateway: None,
                path: &config_path,
                project_desktop: false,
            },
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(root);
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
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
                max_concurrency: 4,
                max_queue_depth: 8,
                queue_timeout_ms: 15_000,
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
            oauth_credentials.as_ref(),
            "api",
            &data_dir,
            None,
            &config_path,
        )
        .await
        .unwrap();

        let document = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let expected_catalog_path = root
            .join("home/.codex/codex-relay-model-catalog.json")
            .display()
            .to_string();
        assert!(result.enabled);
        assert_eq!(result.mode, "third_party");
        assert_eq!(
            result.service_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(result.direct_profile_id.as_deref(), Some("api"));
        assert_eq!(result.direct_profile_alias.as_deref(), Some("Third Party"));
        assert!(result.message.contains("未绑定 OAuth 登录档案"));
        assert!(result.message.contains("模型请求仍走第三方提供商"));
        assert!(!root.join("home/.codex/auth.json").exists());
        assert_eq!(
            document["model_provider"].as_str(),
            Some("codex_relay_direct")
        );
        assert_eq!(document["model"].as_str(), Some("provider-real"));
        assert_eq!(
            document[CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY].as_str(),
            Some(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE)
        );
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some(expected_catalog_path.as_str())
        );
        assert_eq!(
            document["model_providers"]["codex_relay_direct"]["base_url"].as_str(),
            Some("https://api.example.com/v1")
        );
        assert!(document["model_providers"]["codex_relay_direct"]
            .get("requires_openai_auth")
            .is_none());
        assert!(document["model_providers"]["codex_relay_direct"]
            .get("auth")
            .is_some());
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
        assert_eq!(
            catalog["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|level| level["effort"].as_str())
                .collect::<Vec<_>>(),
            vec!["minimal", "low", "medium", "high", "xhigh"]
        );
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
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
                max_concurrency: 4,
                max_queue_depth: 8,
                queue_timeout_ms: 15_000,
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
            oauth_credentials.as_ref(),
            "api",
            &data_dir,
            None,
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
        assert_eq!(
            document[CODEX_CLI_AUTH_CREDENTIALS_STORE_KEY].as_str(),
            Some(CODEX_CLI_AUTH_CREDENTIALS_STORE_FILE)
        );
        assert!(document["model_providers"]["codex_relay"]
            .get("requires_openai_auth")
            .is_none());
        assert!(document["model_providers"]["codex_relay"]
            .get("auth")
            .is_some());
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
        let catalog_path = root.join(CODEX_RELAY_MODEL_CATALOG_FILENAME);
        std::fs::write(&catalog_path, r#"{"models":[]}"#).unwrap();
        std::fs::write(
            &config_path,
            format!(
                r#"
	model_provider = "codex_relay"
	model_catalog_json = "{}"
	cli_auth_credentials_store = "file"

	[model_providers.codex_relay]
name = "Codex Relay LAN Gateway"
base_url = "https://10.12.14.248:53765/v1"
wire_api = "responses"

[model_providers.codex_relay.auth]
command = "/tmp/codex-relay"
args = ["--relay-gateway-token", "client-key:missing", "--relay-data-dir", "{}"]
"#,
                catalog_path.display(),
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
                    validation_status: "unknown".to_owned(),
                    validated_at_ms: None,
                    validation_message: None,
                    max_concurrency: 4,
                    max_queue_depth: 8,
                    queue_timeout_ms: 15_000,
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
                    validation_status: "unknown".to_owned(),
                    validated_at_ms: None,
                    validation_message: None,
                    max_concurrency: 4,
                    max_queue_depth: 8,
                    queue_timeout_ms: 15_000,
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
                    validation_status: "unknown".to_owned(),
                    validated_at_ms: None,
                    validation_message: None,
                    max_concurrency: 4,
                    max_queue_depth: 8,
                    queue_timeout_ms: 15_000,
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
            pool_status: "unavailable".to_owned(),
            direct_route: None,
            active_requests: 0,
            queued_requests: 0,
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
}
