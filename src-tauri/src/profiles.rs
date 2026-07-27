use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{
    database::{Repository, StoredProfile},
    domain::{
        CodexAuthMode, CreateProfileInput, GatewayProvider, MaskedProfile, ProfileAccountSummary,
        ProfileKind, ProfileQuota, ProfileSubscription, UpdateProfileInput,
    },
    error::{AppError, AppResult},
    gateway::normalize_base_url,
    secrets::SecretStore,
};

#[allow(dead_code)]
const KEYCHAIN_SIGNING_MIGRATION_SETTING: &str = "keychain_signing_migration_v2";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexOAuthCredential {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    pub last_refresh_ms: i64,
}

/// Credentials whose native Codex auth.json representation is not an OAuth
/// token bundle. Keeping the generated auth JSON in encrypted local storage lets the runtime
/// preserve Agent Identity and PAT modes without ever putting secrets in SQLite.
#[derive(Clone, Serialize, Deserialize)]
pub struct ImportedAuthFileCredential {
    pub version: u8,
    pub auth_mode: CodexAuthMode,
    pub auth_json: String,
}

impl fmt::Debug for ImportedAuthFileCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedAuthFileCredential")
            .field("version", &self.version)
            .field("auth_mode", &self.auth_mode)
            .field("auth_json", &"<redacted>")
            .finish()
    }
}

impl CodexOAuthCredential {
    pub fn from_auth_json(value: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct AuthFile {
            tokens: Option<AuthTokens>,
        }
        #[derive(Deserialize)]
        struct AuthTokens {
            id_token: Option<String>,
            access_token: Option<String>,
            refresh_token: Option<String>,
            account_id: Option<String>,
        }

        let auth: AuthFile = serde_json::from_str(value).ok()?;
        let tokens = auth.tokens?;
        Some(Self {
            id_token: tokens.id_token?,
            access_token: tokens.access_token?,
            refresh_token: tokens.refresh_token.filter(|value| !value.is_empty()),
            account_id: tokens.account_id,
            last_refresh_ms: timestamp_ms(),
        })
    }

    pub fn auth_json(&self) -> AppResult<String> {
        serde_json::to_string_pretty(&serde_json::json!({
            "auth_mode": null,
            "OPENAI_API_KEY": null,
            "base_url": null,
            "tokens": {
                "id_token": self.id_token,
                "access_token": self.access_token,
                "refresh_token": self.refresh_token.clone().unwrap_or_default(),
                "account_id": self.account_id,
            },
            "agent_identity": null,
            "personal_access_token": null,
            "last_refresh": Utc
                .timestamp_millis_opt(self.last_refresh_ms)
                .single()
                .unwrap_or_else(Utc::now)
                .to_rfc3339_opts(SecondsFormat::Micros, true),
        }))
        .map_err(|_| AppError::Internal)
    }
}

pub async fn create_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    input: CreateProfileInput,
) -> AppResult<MaskedProfile> {
    validate_profile_input(&input)?;
    let id = Uuid::new_v4().to_string();
    let secret_ref = match input.kind {
        ProfileKind::ApiKey => {
            let key = input.api_key.as_deref().ok_or(AppError::ValidationFailed)?;
            let reference = format!("profile:{id}:api_key");
            secrets.set(&reference, key).await?;
            Some(reference)
        }
        ProfileKind::CodexOauth => None,
    };
    let profile = MaskedProfile {
        id,
        alias: input.alias.trim().to_owned(),
        kind: input.kind,
        base_url: input
            .base_url
            .as_deref()
            .map(|url| normalize_base_url(&input.provider, url))
            .transpose()?,
        provider: input.provider,
        enabled: true,
        in_pool: input.in_pool,
        priority: input.priority,
        weight: input.weight,
        models: normalized_models(input.models),
        health: "unknown".to_owned(),
        cooldown_until_ms: None,
        credential_configured: secret_ref.is_some(),
        auth_mode: CodexAuthMode::OAuth,
        is_current: false,
        account: None,
    };
    let stored = StoredProfile {
        profile: profile.clone(),
        secret_ref: secret_ref.clone(),
        credential_fingerprint: None,
    };
    if let Err(error) = repository.insert_profile(&stored) {
        if let Some(reference) = secret_ref {
            let _ = secrets.delete(&reference).await;
        }
        return Err(error);
    }
    Ok(profile)
}

pub async fn update_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    input: UpdateProfileInput,
) -> AppResult<MaskedProfile> {
    if input.alias.trim().is_empty() || input.weight < 1 || input.priority < 0 {
        return Err(AppError::ValidationFailed);
    }
    let mut stored = repository.profile(&input.id)?;
    stored.profile.alias = input.alias.trim().to_owned();
    stored.profile.enabled = input.enabled;
    stored.profile.in_pool = input.in_pool;
    stored.profile.priority = input.priority;
    stored.profile.weight = input.weight;
    stored.profile.models = normalized_models(input.models);
    if let Some(api_key) = input.api_key.filter(|key| !key.trim().is_empty()) {
        if stored.profile.kind != ProfileKind::ApiKey {
            return Err(AppError::ValidationFailed);
        }
        let reference = stored
            .secret_ref
            .clone()
            .unwrap_or_else(|| format!("profile:{}:api_key", stored.profile.id));
        secrets.set(&reference, &api_key).await?;
        stored.secret_ref = Some(reference);
        stored.profile.credential_configured = true;
    }
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

pub async fn save_oauth_credential(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
    credential: &CodexOAuthCredential,
) -> AppResult<MaskedProfile> {
    let stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    let reference = stored
        .secret_ref
        .clone()
        .unwrap_or_else(|| oauth_secret_reference(id));
    let encoded = serde_json::to_string(credential).map_err(|_| AppError::Internal)?;
    secrets.set(&reference, &encoded).await?;
    save_oauth_credential_metadata(repository, id, credential)
}

pub fn save_oauth_credential_metadata(
    repository: &Repository,
    id: &str,
    credential: &CodexOAuthCredential,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    stored.secret_ref = Some(
        stored
            .secret_ref
            .clone()
            .unwrap_or_else(|| oauth_secret_reference(id)),
    );
    stored.profile.credential_configured = true;
    let previous_account = stored.profile.account.clone();
    stored.profile.account = account_summary(credential).map(|mut account| {
        if let Some(previous) = previous_account {
            account.quota = previous.quota;
            account.subscription = previous.subscription;
        }
        account
    });
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

#[cfg(test)]
pub async fn sync_oauth_account_info(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    let reference = stored
        .secret_ref
        .as_deref()
        .ok_or(AppError::ProfileRuntimeUnavailable)?;
    let value = secrets.get(reference).await.map_err(|error| match error {
        AppError::NotFound => AppError::ProfileRuntimeUnavailable,
        other => other,
    })?;
    let credential =
        serde_json::from_str(&value).map_err(|_| AppError::ProfileRuntimeUnavailable)?;
    stored.profile.account = account_summary(&credential);
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

pub fn sync_oauth_account_info_with_snapshot(
    repository: &Repository,
    id: &str,
    credential: &CodexOAuthCredential,
    quota: ProfileQuota,
    subscription: ProfileSubscription,
    runtime_email: Option<String>,
    runtime_account_id: Option<String>,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    let previous_account = stored.profile.account.clone();
    stored.profile.account = account_summary_with_snapshot(credential, quota, subscription)
        .map(|mut account| {
            account.email = normalized_account_value(runtime_email, 320).or(account.email);
            account.account_id =
                normalized_account_value(runtime_account_id, 160).or(account.account_id);
            if let Some(previous) = previous_account.as_ref() {
                if account.subscription.status != "available"
                    && (previous.subscription.plan_type.is_some()
                        || previous.subscription.period_ends_at_ms.is_some())
                {
                    let last_attempt_at_ms = account.subscription.last_attempt_at_ms;
                    let last_error = account.subscription.last_error.clone();
                    account.subscription = previous.subscription.clone();
                    account.subscription.status = "stale".to_owned();
                    account.subscription.last_attempt_at_ms = last_attempt_at_ms;
                    account.subscription.last_error = last_error
                        .or_else(|| Some("订阅周期同步未完成，正在保留最近一次结果。".to_owned()));
                }
            }
            account
        })
        .or(previous_account);
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

pub fn sync_codex_account_info_with_snapshot(
    repository: &Repository,
    id: &str,
    quota: ProfileQuota,
    subscription: ProfileSubscription,
    runtime_email: Option<String>,
    runtime_account_id: Option<String>,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    let previous_account = stored.profile.account.clone();
    let now = timestamp_ms();
    let mut account = previous_account.clone().unwrap_or(ProfileAccountSummary {
        display_name: None,
        email: None,
        account_id: None,
        updated_at_ms: now,
        quota: unavailable_quota("额度尚未同步。"),
        subscription: unavailable_subscription("订阅资料尚未同步。"),
    });
    account.updated_at_ms = now;
    account.email = normalized_account_value(runtime_email, 320).or(account.email);
    account.account_id = normalized_account_value(runtime_account_id, 160).or(account.account_id);
    account.quota = quota;
    if subscription.status == "unavailable"
        && previous_account.as_ref().is_some_and(|previous| {
            previous.subscription.plan_type.is_some()
                || previous.subscription.period_ends_at_ms.is_some()
        })
    {
        let last_attempt_at_ms = subscription.last_attempt_at_ms;
        let last_error = subscription.last_error.clone();
        account.subscription = previous_account
            .as_ref()
            .expect("checked above")
            .subscription
            .clone();
        account.subscription.status = "stale".to_owned();
        account.subscription.last_attempt_at_ms = last_attempt_at_ms;
        account.subscription.last_error =
            last_error.or_else(|| Some("订阅周期同步未完成，正在保留最近一次结果。".to_owned()));
    } else {
        account.subscription = subscription;
    }
    stored.profile.account = Some(account);
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

pub fn mark_quota_stale(repository: &Repository, id: &str) -> AppResult<MaskedProfile> {
    mark_quota_stale_with_message(
        repository,
        id,
        "同步未完成",
        "额度数据暂未更新，正在保留最近一次同步结果。",
        "额度暂不可用，请稍后重试。",
    )
}

pub fn mark_quota_stale_for_keychain_interaction(
    repository: &Repository,
    id: &str,
) -> AppResult<MaskedProfile> {
    mark_quota_stale_with_message(
        repository,
        id,
        "后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。",
        "后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。",
        "后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。",
    )
}

fn mark_quota_stale_with_message(
    repository: &Repository,
    id: &str,
    error: &str,
    stale_message: &str,
    unavailable_message: &str,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    let now = timestamp_ms();
    let account = stored.profile.account.get_or_insert(ProfileAccountSummary {
        display_name: None,
        email: None,
        account_id: None,
        updated_at_ms: now,
        quota: unavailable_quota("额度暂不可用，请稍后重试。"),
        subscription: unavailable_subscription("订阅资料尚未同步。"),
    });
    account.quota.last_attempt_at_ms = now;
    account.quota.last_error = Some(error.to_owned());
    if account.quota.primary.is_some() || account.quota.secondary.is_some() {
        account.quota.status = "stale".to_owned();
        account.quota.message = stale_message.to_owned();
    } else {
        account.quota.status = "unavailable".to_owned();
        account.quota.message = unavailable_message.to_owned();
    }
    account.subscription.last_attempt_at_ms = now;
    account.subscription.last_error = Some(error.to_owned());
    if account.subscription.plan_type.is_some() || account.subscription.period_ends_at_ms.is_some()
    {
        account.subscription.status = "stale".to_owned();
    } else {
        account.subscription.status = "unavailable".to_owned();
    }
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

pub async fn create_oauth_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: String,
    alias: String,
    credential: &CodexOAuthCredential,
) -> AppResult<MaskedProfile> {
    if alias.trim().is_empty() {
        return Err(AppError::ValidationFailed);
    }
    let reference = oauth_secret_reference(&id);
    let encoded = serde_json::to_string(credential).map_err(|_| AppError::Internal)?;
    secrets.set(&reference, &encoded).await?;
    let profile = MaskedProfile {
        id: id.clone(),
        alias: alias.trim().to_owned(),
        kind: ProfileKind::CodexOauth,
        base_url: None,
        provider: GatewayProvider::OpenAi,
        enabled: true,
        in_pool: false,
        priority: 0,
        weight: 1,
        models: Vec::new(),
        health: "unknown".to_owned(),
        cooldown_until_ms: None,
        credential_configured: true,
        auth_mode: CodexAuthMode::OAuth,
        is_current: false,
        account: account_summary(credential),
    };
    if let Err(error) = repository.insert_profile(&StoredProfile {
        profile: profile.clone(),
        secret_ref: Some(reference.clone()),
        credential_fingerprint: None,
    }) {
        let _ = secrets.delete(&reference).await;
        return Err(error);
    }
    Ok(profile)
}

pub async fn create_imported_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    alias: String,
    credential: &ImportedAuthFileCredential,
    fingerprint: String,
    account: Option<ProfileAccountSummary>,
) -> AppResult<MaskedProfile> {
    if alias.trim().is_empty() {
        return Err(AppError::ValidationFailed);
    }
    let id = Uuid::new_v4().to_string();
    let reference = oauth_secret_reference(&id);
    let encoded = serde_json::to_string(credential).map_err(|_| AppError::Internal)?;
    secrets.set(&reference, &encoded).await?;
    let profile = MaskedProfile {
        id: id.clone(),
        alias: alias.trim().to_owned(),
        kind: ProfileKind::CodexOauth,
        base_url: None,
        provider: GatewayProvider::OpenAi,
        enabled: true,
        in_pool: false,
        priority: 0,
        weight: 1,
        models: Vec::new(),
        health: "unknown".to_owned(),
        cooldown_until_ms: None,
        credential_configured: true,
        auth_mode: credential.auth_mode.clone(),
        is_current: false,
        account,
    };
    if let Err(error) = repository.insert_profile(&StoredProfile {
        profile: profile.clone(),
        secret_ref: Some(reference.clone()),
        credential_fingerprint: Some(fingerprint),
    }) {
        let _ = secrets.delete(&reference).await;
        return Err(error);
    }
    Ok(profile)
}

pub async fn update_imported_profile_credential(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
    credential: &ImportedAuthFileCredential,
    fingerprint: String,
    account: Option<ProfileAccountSummary>,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::CodexOauth {
        return Err(AppError::ValidationFailed);
    }
    let reference = stored
        .secret_ref
        .clone()
        .unwrap_or_else(|| oauth_secret_reference(id));
    let previous = if stored.secret_ref.is_some() {
        secrets.get(&reference).await.ok()
    } else {
        None
    };
    let encoded = serde_json::to_string(credential).map_err(|_| AppError::Internal)?;
    secrets.set(&reference, &encoded).await?;
    stored.secret_ref = Some(reference.clone());
    stored.credential_fingerprint = Some(fingerprint);
    stored.profile.credential_configured = true;
    stored.profile.auth_mode = credential.auth_mode.clone();
    if account.is_some() {
        stored.profile.account = account;
    }
    if let Err(error) = repository.update_profile(&stored) {
        match previous {
            Some(value) => {
                let _ = secrets.set(&reference, &value).await;
            }
            None => {
                let _ = secrets.delete(&reference).await;
            }
        }
        return Err(error);
    }
    Ok(stored.profile)
}

pub fn imported_account_summary(
    email: Option<String>,
    account_id: Option<String>,
    plan_type: Option<String>,
) -> Option<ProfileAccountSummary> {
    let display_name = None;
    let email = normalized_account_value(email, 320);
    let account_id = normalized_account_value(account_id, 160);
    let plan_type = normalized_account_value(plan_type, 120);
    if email.is_none() && account_id.is_none() && plan_type.is_none() {
        return None;
    }
    let updated_at_ms = timestamp_ms();
    let subscription = match plan_type {
        Some(plan_type) => ProfileSubscription {
            status: "available".to_owned(),
            plan_type: Some(plan_type),
            period_ends_at_ms: None,
            will_renew: None,
            source: Some("import_verification".to_owned()),
            synced_at_ms: Some(updated_at_ms),
            last_attempt_at_ms: updated_at_ms,
            last_error: None,
        },
        None => unavailable_subscription("订阅资料尚未同步。"),
    };
    Some(ProfileAccountSummary {
        display_name,
        email,
        account_id,
        updated_at_ms,
        quota: unavailable_quota("额度尚未同步。"),
        subscription,
    })
}

#[allow(dead_code)]
pub async fn migrate_oauth_credentials(repository: &Repository, stable_app_signature: bool) {
    let signing_migration_pending = stable_app_signature
        && repository
            .setting(KEYCHAIN_SIGNING_MIGRATION_SETTING)
            .ok()
            .flatten()
            .is_none();
    if !signing_migration_pending {
        return;
    }
    let profiles = match repository.list_profiles() {
        Ok(profiles) => profiles,
        Err(_) => return,
    };
    for mut stored in profiles
        .into_iter()
        .filter(|stored| stored.profile.kind == ProfileKind::CodexOauth)
    {
        if stored
            .secret_ref
            .as_deref()
            .is_some_and(|reference| reference.ends_with(":oauth:v2"))
        {
            continue;
        }
        // Do not read old Keychain entries here. macOS may show an unlock prompt
        // merely to inspect a legacy item, which makes application startup noisy.
        // A new v2 reference keeps the old entry untouched and reauthorization
        // creates a credential associated with the stable application signature.
        stored.secret_ref = Some(oauth_secret_reference(&stored.profile.id));
        stored.profile.credential_configured = false;
        let _ = repository.update_profile(&stored);
    }
    let _ = repository.set_setting(KEYCHAIN_SIGNING_MIGRATION_SETTING, "complete");
}

fn oauth_secret_reference(id: &str) -> String {
    format!("profile:{id}:oauth:v2")
}

#[derive(Deserialize)]
struct IdTokenClaims {
    name: Option<String>,
    email: Option<String>,
    sub: Option<String>,
}

fn account_summary(credential: &CodexOAuthCredential) -> Option<ProfileAccountSummary> {
    account_summary_with_snapshot(
        credential,
        unavailable_quota("额度尚未同步。"),
        unavailable_subscription("订阅资料尚未同步。"),
    )
    .filter(|account| {
        account.display_name.is_some() || account.email.is_some() || account.account_id.is_some()
    })
}

fn account_summary_with_snapshot(
    credential: &CodexOAuthCredential,
    quota: ProfileQuota,
    subscription: ProfileSubscription,
) -> Option<ProfileAccountSummary> {
    let claims = credential
        .id_token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<IdTokenClaims>(&payload).ok());
    let display_name = claims
        .as_ref()
        .and_then(|claims| normalized_account_value(claims.name.clone(), 120));
    let email = claims
        .as_ref()
        .and_then(|claims| normalized_account_value(claims.email.clone(), 320));
    let account_id = normalized_account_value(credential.account_id.clone(), 160)
        .or_else(|| claims.and_then(|claims| normalized_account_value(claims.sub, 160)));
    if display_name.is_none()
        && email.is_none()
        && account_id.is_none()
        && quota.status != "available"
        && subscription.plan_type.is_none()
    {
        return None;
    }
    Some(ProfileAccountSummary {
        display_name,
        email,
        account_id,
        updated_at_ms: timestamp_ms(),
        quota,
        subscription,
    })
}

pub fn unavailable_quota(message: impl Into<String>) -> ProfileQuota {
    ProfileQuota {
        status: "unavailable".to_owned(),
        message: message.into(),
        source: None,
        synced_at_ms: None,
        last_attempt_at_ms: timestamp_ms(),
        last_error: None,
        primary: None,
        secondary: None,
        buckets: Vec::new(),
        rate_limit_reached_type: None,
    }
}

pub fn unavailable_subscription(message: impl Into<String>) -> ProfileSubscription {
    ProfileSubscription {
        status: "unavailable".to_owned(),
        plan_type: None,
        period_ends_at_ms: None,
        will_renew: None,
        source: None,
        synced_at_ms: None,
        last_attempt_at_ms: timestamp_ms(),
        last_error: Some(message.into()),
    }
}

fn normalized_account_value(value: Option<String>, max_chars: usize) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.chars().take(max_chars).collect())
    })
}

pub async fn delete_profile(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
    confirmed: bool,
) -> AppResult<()> {
    if !confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let secret_ref = repository.delete_profile(id)?;
    if let Some(reference) = secret_ref {
        secrets.delete(&reference).await?;
    }
    Ok(())
}

pub fn select_current_profile(repository: &Repository, id: &str) -> AppResult<()> {
    repository.set_current_profile(id)
}

pub fn candidates_for_model(
    repository: &Repository,
    model: Option<&str>,
) -> AppResult<Vec<StoredProfile>> {
    let now = timestamp_ms();
    let mut candidates = repository
        .list_profiles()?
        .into_iter()
        .filter(|stored| {
            let profile = &stored.profile;
            profile.enabled
                && profile.in_pool
                && (profile.kind == ProfileKind::ApiKey
                    || (profile.kind == ProfileKind::CodexOauth
                        && profile.auth_mode == CodexAuthMode::OAuth))
                && profile.credential_configured
                && !matches!(
                    profile.health.as_str(),
                    "unhealthy" | "reauthorization_required"
                )
                && profile.cooldown_until_ms.is_none_or(|until| until <= now)
                && model
                    .is_none_or(|model| profile.models.iter().any(|candidate| candidate == model))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.profile.priority);
    Ok(candidates)
}

pub fn cool_down_profile(repository: &Repository, id: &str, duration: std::time::Duration) {
    let Ok(mut stored) = repository.profile(id) else {
        return;
    };
    stored.profile.cooldown_until_ms =
        Some(timestamp_ms().saturating_add(duration.as_millis().try_into().unwrap_or(i64::MAX)));
    let _ = repository.update_profile(&stored);
}

pub fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn validate_profile_input(input: &CreateProfileInput) -> AppResult<()> {
    if input.alias.trim().is_empty() || input.weight < 1 || input.priority < 0 {
        return Err(AppError::ValidationFailed);
    }
    match input.kind {
        ProfileKind::ApiKey => {
            if input.in_pool && input.models.is_empty() {
                return Err(AppError::ValidationFailed);
            }
            let url = input
                .base_url
                .as_deref()
                .ok_or(AppError::ValidationFailed)?;
            let parsed = Url::parse(url).map_err(|_| AppError::ValidationFailed)?;
            if !valid_upstream_scheme(&input.provider, &parsed)
                || parsed.host_str().is_none()
                || input
                    .api_key
                    .as_deref()
                    .is_none_or(|key| key.trim().is_empty())
            {
                return Err(AppError::ValidationFailed);
            }
        }
        ProfileKind::CodexOauth => {
            return Err(AppError::ValidationFailed);
        }
    }
    Ok(())
}

fn valid_upstream_scheme(provider: &GatewayProvider, parsed: &Url) -> bool {
    if parsed.scheme() == "https" {
        return true;
    }
    if parsed.scheme() != "http" {
        return false;
    }
    matches!(
        provider,
        GatewayProvider::Ollama | GatewayProvider::OpenAiCompatible
    ) && parsed
        .host_str()
        .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "::1")
}

fn normalized_models(models: Vec<String>) -> Vec<String> {
    let mut values = models
        .into_iter()
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::{
        candidates_for_model, create_oauth_profile, create_profile, imported_account_summary,
        migrate_oauth_credentials, sync_oauth_account_info, sync_oauth_account_info_with_snapshot,
        unavailable_quota, CodexOAuthCredential, KEYCHAIN_SIGNING_MIGRATION_SETTING,
    };
    use crate::{
        database::Repository,
        domain::{CreateProfileInput, GatewayProvider, ProfileKind, ProfileSubscription},
        secrets::MemorySecretStore,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use std::sync::Arc;

    fn id_token(claims: serde_json::Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    #[test]
    fn imported_identity_summary_keeps_only_non_sensitive_display_fields() {
        let summary = imported_account_summary(
            Some("imported@example.com".into()),
            Some("account-imported".into()),
            Some("pro".into()),
        )
        .expect("identity metadata should be retained");

        assert_eq!(summary.email.as_deref(), Some("imported@example.com"));
        assert_eq!(summary.account_id.as_deref(), Some("account-imported"));
        assert_eq!(summary.subscription.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            summary.subscription.source.as_deref(),
            Some("import_verification")
        );
        assert_eq!(summary.quota.status, "unavailable");
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(!serialized.contains("access_token"));
        assert!(!serialized.contains("refresh_token"));
    }

    #[tokio::test]
    async fn never_persists_a_profile_when_secret_storage_fails() {
        struct FailingStore;
        #[async_trait::async_trait]
        impl crate::secrets::SecretStore for FailingStore {
            async fn set(&self, _: &str, _: &str) -> crate::error::AppResult<()> {
                Err(crate::error::AppError::SecretStoreUnavailable)
            }
            async fn get(&self, _: &str) -> crate::error::AppResult<String> {
                Err(crate::error::AppError::SecretStoreUnavailable)
            }
            async fn delete(&self, _: &str) -> crate::error::AppResult<()> {
                Ok(())
            }
        }
        let repository = Repository::memory();
        let result = create_profile(
            &repository,
            Arc::new(FailingStore),
            CreateProfileInput {
                alias: "Primary".into(),
                kind: ProfileKind::ApiKey,
                base_url: Some("https://relay.example.com/v1".into()),
                provider: GatewayProvider::OpenAiCompatible,
                api_key: Some("secret".into()),
                models: vec!["gpt-5-codex".into()],
                in_pool: true,
                priority: 0,
                weight: 1,
            },
        )
        .await;
        assert!(result.is_err());
        assert!(repository.list_profiles().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stores_only_a_secret_reference_in_sqlite() {
        let repository = Repository::memory();
        let _ = create_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            CreateProfileInput {
                alias: "Primary".into(),
                kind: ProfileKind::ApiKey,
                base_url: Some("https://relay.example.com/v1".into()),
                provider: GatewayProvider::OpenAiCompatible,
                api_key: Some("super-secret".into()),
                models: vec!["gpt-5-codex".into()],
                in_pool: true,
                priority: 0,
                weight: 1,
            },
        )
        .await
        .unwrap();
        let profile = repository.list_profiles().unwrap().remove(0);
        assert!(profile.secret_ref.unwrap().starts_with("profile:"));
        assert!(profile.profile.credential_configured);
    }

    #[tokio::test]
    async fn oauth_profile_persists_a_keychain_reference_and_no_models() {
        let repository = Repository::memory();
        let created = create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "oauth-id".into(),
            "Personal".into(),
            &CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();
        let stored = repository.profile("oauth-id").unwrap();

        assert_eq!(created.kind, ProfileKind::CodexOauth);
        assert!(created.credential_configured);
        assert!(!created.in_pool);
        assert!(created.models.is_empty());
        assert_eq!(
            stored.secret_ref.as_deref(),
            Some("profile:oauth-id:oauth:v2")
        );
    }

    #[tokio::test]
    async fn gateway_candidates_include_only_opted_in_oauth_profiles() {
        let repository = Repository::memory();
        let secrets = Arc::new(MemorySecretStore::new());
        let api_profile = create_profile(
            &repository,
            secrets.clone(),
            CreateProfileInput {
                alias: "Verified API".into(),
                kind: ProfileKind::ApiKey,
                base_url: Some("https://api.example.com/v1".into()),
                provider: GatewayProvider::OpenAi,
                api_key: Some("api-secret".into()),
                models: vec!["gpt-5".into()],
                in_pool: true,
                priority: 0,
                weight: 1,
            },
        )
        .await
        .unwrap();
        let oauth_profile = create_oauth_profile(
            &repository,
            secrets,
            "oauth-id".into(),
            "Personal OAuth".into(),
            &CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: None,
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();

        assert!(candidates_for_model(&repository, Some("gpt-5"))
            .unwrap()
            .iter()
            .all(|candidate| candidate.profile.id != oauth_profile.id));

        let mut opted_in_oauth = repository.profile(&oauth_profile.id).unwrap();
        opted_in_oauth.profile.in_pool = true;
        opted_in_oauth.profile.models = vec!["gpt-5".into()];
        opted_in_oauth.profile.health = "healthy".into();
        repository.update_profile(&opted_in_oauth).unwrap();

        let candidates = candidates_for_model(&repository, Some("gpt-5")).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates
            .iter()
            .any(|candidate| candidate.profile.id == api_profile.id));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.profile.id == oauth_profile.id));
    }

    #[tokio::test]
    async fn stores_only_selected_oidc_claims_in_the_profile_account_summary() {
        let repository = Repository::memory();
        let id_token = id_token(serde_json::json!({
            "name": "Ada Lovelace",
            "email": "ada@example.com",
            "sub": "subject_123",
            "unrelated": "must not be stored",
        }));
        let created = create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "oauth-id".into(),
            "Personal".into(),
            &CodexOAuthCredential {
                id_token: id_token.clone(),
                access_token: "access-token".into(),
                refresh_token: Some("refresh-token".into()),
                account_id: Some("account_456".into()),
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();

        let account = created
            .account
            .as_ref()
            .expect("recognized OIDC claims must be retained");
        assert_eq!(account.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(account.email.as_deref(), Some("ada@example.com"));
        assert_eq!(account.account_id.as_deref(), Some("account_456"));
        assert_eq!(account.quota.status, "unavailable");

        let serialized = serde_json::to_string(&created).unwrap();
        assert!(!serialized.contains(&id_token));
        assert!(!serialized.contains("access-token"));
        assert!(!serialized.contains("refresh-token"));
        assert!(!serialized.contains("must not be stored"));
    }

    #[tokio::test]
    async fn malformed_id_tokens_leave_account_metadata_empty_without_blocking_oauth_storage() {
        let repository = Repository::memory();
        let created = create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "oauth-id".into(),
            "Personal".into(),
            &CodexOAuthCredential {
                id_token: "not-a-jwt".into(),
                access_token: "access".into(),
                refresh_token: None,
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();

        assert!(created.account.is_none());
        assert!(created.credential_configured);
    }

    #[tokio::test]
    async fn user_requested_account_sync_reads_the_saved_oauth_credential() {
        let repository = Repository::memory();
        let store = Arc::new(MemorySecretStore::new());
        let credential = CodexOAuthCredential {
            id_token: id_token(serde_json::json!({"email": "ada@example.com", "sub": "sub_123"})),
            access_token: "access".into(),
            refresh_token: None,
            account_id: None,
            last_refresh_ms: 1,
        };
        create_oauth_profile(
            &repository,
            store.clone(),
            "oauth-id".into(),
            "Personal".into(),
            &credential,
        )
        .await
        .unwrap();
        let mut stored = repository.profile("oauth-id").unwrap();
        stored.profile.account = None;
        repository.update_profile(&stored).unwrap();

        let synced = sync_oauth_account_info(&repository, store, "oauth-id")
            .await
            .unwrap();
        assert_eq!(
            synced.account.and_then(|account| account.email),
            Some("ada@example.com".into())
        );
    }

    #[tokio::test]
    async fn stale_subscription_sync_preserves_the_last_successful_cycle() {
        let repository = Repository::memory();
        let credential = CodexOAuthCredential {
            id_token: id_token(serde_json::json!({"email": "ada@example.com"})),
            access_token: "access".into(),
            refresh_token: None,
            account_id: Some("account_123".into()),
            last_refresh_ms: 1,
        };
        create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "oauth-id".into(),
            "Personal".into(),
            &credential,
        )
        .await
        .unwrap();
        let available = ProfileSubscription {
            status: "available".into(),
            plan_type: Some("pro".into()),
            period_ends_at_ms: Some(1_785_542_400_000),
            will_renew: Some(true),
            source: Some("account_check".into()),
            synced_at_ms: Some(10),
            last_attempt_at_ms: 10,
            last_error: None,
        };
        sync_oauth_account_info_with_snapshot(
            &repository,
            "oauth-id",
            &credential,
            unavailable_quota("quota unavailable"),
            available,
            None,
            None,
        )
        .unwrap();

        let stale = sync_oauth_account_info_with_snapshot(
            &repository,
            "oauth-id",
            &credential,
            unavailable_quota("quota unavailable"),
            ProfileSubscription {
                status: "unavailable".into(),
                plan_type: None,
                period_ends_at_ms: None,
                will_renew: None,
                source: None,
                synced_at_ms: None,
                last_attempt_at_ms: 20,
                last_error: Some("network failed".into()),
            },
            None,
            None,
        )
        .unwrap();

        let subscription = &stale.account.unwrap().subscription;
        assert_eq!(subscription.status, "stale");
        assert_eq!(subscription.plan_type.as_deref(), Some("pro"));
        assert_eq!(subscription.period_ends_at_ms, Some(1_785_542_400_000));
        assert_eq!(subscription.last_attempt_at_ms, 20);
        assert_eq!(subscription.last_error.as_deref(), Some("network failed"));
    }

    #[tokio::test]
    async fn account_sync_surfaces_keychain_failures() {
        struct FailingReadStore;
        #[async_trait::async_trait]
        impl crate::secrets::SecretStore for FailingReadStore {
            async fn set(&self, _: &str, _: &str) -> crate::error::AppResult<()> {
                Ok(())
            }
            async fn get(&self, _: &str) -> crate::error::AppResult<String> {
                Err(crate::error::AppError::SecretStoreUnavailable)
            }
            async fn delete(&self, _: &str) -> crate::error::AppResult<()> {
                Ok(())
            }
        }

        let repository = Repository::memory();
        create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "oauth-id".into(),
            "Personal".into(),
            &CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: None,
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();

        let error = sync_oauth_account_info(&repository, Arc::new(FailingReadStore), "oauth-id")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::error::AppError::SecretStoreUnavailable
        ));
    }

    #[tokio::test]
    async fn oauth_cannot_be_created_before_official_authorization() {
        let repository = Repository::memory();
        let result = create_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            CreateProfileInput {
                alias: "Personal".into(),
                kind: ProfileKind::CodexOauth,
                base_url: None,
                provider: GatewayProvider::OpenAiCompatible,
                api_key: None,
                models: vec!["unknown".into()],
                in_pool: false,
                priority: 0,
                weight: 1,
            },
        )
        .await;

        assert!(result.is_err());
        assert!(repository.list_profiles().unwrap().is_empty());
    }

    #[tokio::test]
    async fn marks_legacy_oauth_credentials_for_reauthorization_without_reading_keychain() {
        let repository = Repository::memory();
        let credential = CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: None,
            last_refresh_ms: 1,
        };
        create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "oauth-id".into(),
            "Personal".into(),
            &credential,
        )
        .await
        .unwrap();

        // This simulates a pre-v2 reference created before the stable signing
        // migration. The migration must not touch the old Keychain item.
        let mut stored = repository.profile("oauth-id").unwrap();
        stored.secret_ref = Some("profile:oauth-id:oauth".into());
        repository.update_profile(&stored).unwrap();

        migrate_oauth_credentials(&repository, true).await;

        let stored = repository.profile("oauth-id").unwrap();
        assert_eq!(
            stored.secret_ref.as_deref(),
            Some("profile:oauth-id:oauth:v2")
        );
        assert!(!stored.profile.credential_configured);
        assert_eq!(
            repository
                .setting(KEYCHAIN_SIGNING_MIGRATION_SETTING)
                .unwrap()
                .as_deref(),
            Some("complete")
        );

        migrate_oauth_credentials(&repository, true).await;
        assert_eq!(
            repository
                .profile("oauth-id")
                .unwrap()
                .secret_ref
                .as_deref(),
            Some("profile:oauth-id:oauth:v2")
        );
    }
}
