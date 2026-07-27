use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{value, Array, DocumentMut, Item, Table};
use uuid::Uuid;

use crate::{
    database::Repository,
    domain::{
        GatewayCodexConfigStatus, GatewayStatus, MaskedClientKey,
        GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
    },
    error::{AppError, AppResult},
    profiles::timestamp_ms,
    secrets::SecretStore,
};

const SNAPSHOT_REF: &str = "gateway:codex-config-snapshot";
const CODEX_CLIENT_KEY_NAME: &str = "Codex CLI Gateway";

#[derive(Serialize, Deserialize)]
struct ConfigSnapshot {
    original: String,
    generated_hash: String,
}

pub async fn status() -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    status_for_path(&path).await
}

async fn status_for_path(path: &Path) -> AppResult<GatewayCodexConfigStatus> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let document = content.parse::<DocumentMut>().ok();
    let provider = document
        .as_ref()
        .and_then(|document| document.get("model_provider")?.as_str().map(str::to_owned));
    let enabled = matches!(
        provider.as_deref(),
        Some("codex_relay" | "codex_relay_direct")
    );
    let auth_config = document
        .as_ref()
        .and_then(|document| relay_auth_config(document, provider.as_deref()?));
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
    Ok(GatewayCodexConfigStatus {
        enabled,
        config_path: path.display().to_string(),
        service_url: None,
        auth_status: auth_status.to_owned(),
        needs_repair: matches!(auth_status, "legacy" | "invalid"),
        message: if enabled && auth_config.is_none() {
            "Codex Relay 网关配置来自旧凭据存储；请重新启用网关配置以使用本地凭据库。".to_owned()
        } else if enabled && !auth_token_readable {
            "Codex Relay 网关 Client Key 已失效；请重新启用网关配置以自动修复 Codex Client Key。"
                .to_owned()
        } else if enabled {
            "Codex 正在使用 Relay 网关配置。".to_owned()
        } else {
            "Codex 尚未切换到 Relay 网关。".to_owned()
        },
    })
}

pub async fn enable(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    let path = config_path()?;
    enable_for_path(repository, secrets, gateway, data_dir, &path).await
}

async fn enable_for_path(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
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
    let secret_ref = ensure_codex_client_key(repository, secrets.clone()).await?;
    enable_at_path(secrets, gateway, data_dir, path, &secret_ref).await
}

async fn enable_at_path(
    secrets: Arc<dyn SecretStore>,
    gateway: GatewayStatus,
    data_dir: &Path,
    path: &Path,
    secret_ref: &str,
) -> AppResult<GatewayCodexConfigStatus> {
    let original = fs::read_to_string(path).unwrap_or_default();
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    document["model_provider"] = value("codex_relay");
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
    providers.insert("codex_relay", Item::Table(provider));
    let generated = document.to_string();
    let snapshot = ConfigSnapshot {
        original,
        generated_hash: content_hash(&generated),
    };
    secrets
        .set(
            SNAPSHOT_REF,
            &serde_json::to_string(&snapshot).map_err(|_| AppError::Internal)?,
        )
        .await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    fs::write(path, generated).map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(GatewayCodexConfigStatus {
        enabled: true,
        config_path: path.display().to_string(),
        service_url: Some(gateway.service_url),
        auth_status: "ok".to_owned(),
        needs_repair: false,
        message: "Codex 已切换到 Relay 网关。请在系统中信任 Relay 导出的 CA 后启动新会话。"
            .to_owned(),
    })
}

async fn ensure_codex_client_key(
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
    id: &str,
    data_dir: &Path,
) -> AppResult<GatewayCodexConfigStatus> {
    let stored = repository.profile(id)?;
    if stored.profile.kind != crate::domain::ProfileKind::ApiKey
        || !matches!(
            stored.profile.provider,
            crate::domain::GatewayProvider::OpenAi
                | crate::domain::GatewayProvider::OpenAiCompatible
        )
        || stored.profile.health != "healthy"
    {
        return Err(AppError::UpstreamUnavailable);
    }
    let base_url = stored.profile.base_url.ok_or(AppError::ValidationFailed)?;
    let secret_ref = stored.secret_ref.ok_or(AppError::ValidationFailed)?;
    let model = stored
        .profile
        .models
        .first()
        .cloned()
        .ok_or(AppError::UpstreamUnavailable)?;
    let path = config_path()?;
    let original = fs::read_to_string(&path).unwrap_or_default();
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    document["model_provider"] = value("codex_relay_direct");
    document["model"] = value(model);
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
    push_relay_auth_args(&mut args, secret_ref.as_str(), data_dir);
    auth["args"] = Item::Value(args.into());
    provider["auth"] = Item::Table(auth);
    providers.insert("codex_relay_direct", Item::Table(provider));
    let generated = document.to_string();
    let snapshot = ConfigSnapshot {
        original,
        generated_hash: content_hash(&generated),
    };
    secrets
        .set(
            SNAPSHOT_REF,
            &serde_json::to_string(&snapshot).map_err(|_| AppError::Internal)?,
        )
        .await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    fs::write(&path, generated).map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(GatewayCodexConfigStatus {
        enabled: true,
        config_path: path.display().to_string(),
        service_url: None,
        auth_status: "ok".to_owned(),
        needs_repair: false,
        message: "Codex 已切换到已验证的 API 服务档案。".to_owned(),
    })
}

pub async fn disable(secrets: Arc<dyn SecretStore>) -> AppResult<GatewayCodexConfigStatus> {
    let snapshot: ConfigSnapshot =
        serde_json::from_str(&secrets.get(SNAPSHOT_REF).await?).map_err(|_| AppError::Internal)?;
    let path = config_path()?;
    let current = fs::read_to_string(&path).unwrap_or_default();
    if content_hash(&current) == snapshot.generated_hash {
        fs::write(&path, snapshot.original).map_err(|_| AppError::RuntimeUnavailable)?;
        secrets.delete(SNAPSHOT_REF).await?;
        return status().await;
    }
    let mut current_document = current
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    let original_document = snapshot
        .original
        .parse::<DocumentMut>()
        .map_err(|_| AppError::ValidationFailed)?;
    remove_relay_config(&mut current_document, &original_document);
    fs::write(&path, current_document.to_string()).map_err(|_| AppError::RuntimeUnavailable)?;
    secrets.delete(SNAPSHOT_REF).await?;
    let mut result = status().await?;
    result.message = "已移除 Relay 配置；保留了启用期间的其他 Codex 配置改动。".to_owned();
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
    if matches!(
        current.get("model_provider").and_then(Item::as_str),
        Some("codex_relay" | "codex_relay_direct")
    ) {
        if let Some(original_provider) = original.get("model_provider") {
            current["model_provider"] = original_provider.clone();
        } else {
            current.as_table_mut().remove("model_provider");
        }
    }
    if let Some(providers) = current
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove("codex_relay");
        providers.remove("codex_relay_direct");
    }
}

fn config_path() -> AppResult<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(AppError::RuntimeUnavailable)?;
    Ok(PathBuf::from(home).join(".codex/config.toml"))
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
        content_hash, enable_for_path, relay_auth_config, remove_relay_config, status_for_path,
    };
    use crate::{
        database::Repository,
        domain::{
            GatewayNetworkAddress, GatewayStatus, MaskedClientKey,
            GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
        },
        secrets::LocalEncryptedSecretStore,
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
        let original = "model_provider = 'openai'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut current = "model_provider = 'codex_relay'\n[model_providers.codex_relay]\nbase_url = 'https://relay'\n[custom]\nvalue = 'keep'\n"
            .parse::<DocumentMut>()
            .unwrap();
        remove_relay_config(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("openai"));
        assert!(current["model_providers"].get("codex_relay").is_none());
        assert_eq!(current["custom"]["value"].as_str(), Some("keep"));
    }

    #[tokio::test]
    async fn enable_repairs_a_missing_codex_client_key_secret() {
        let repository = Repository::memory();
        insert_stale_client_key(&repository);
        repository
            .set_setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, "client-key:stale")
            .unwrap();
        let root = temp_root("codex-gateway-enable-repair");
        let data_dir = root.join("data");
        let secrets = Arc::new(LocalEncryptedSecretStore::open(&data_dir).unwrap());
        let config_path = root.join("home/.codex/config.toml");

        let result = enable_for_path(
            &repository,
            secrets,
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

        let status = status_for_path(&config_path).await.unwrap();

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
