use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    database::StoredProfile,
    domain::{CodexAuthMode, ProfileKind},
    error::{AppError, AppResult},
    profiles::{timestamp_ms, CodexOAuthCredential, ImportedAuthFileCredential},
    secrets::SecretStore,
};

pub(crate) const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialAccess {
    Background,
    UserInitiated,
}

#[derive(Clone)]
struct CachedCredential {
    credential: CodexOAuthCredential,
    persistence_pending: bool,
}

pub(crate) struct OAuthCredentialStore {
    secrets: Arc<dyn SecretStore>,
    cache: Mutex<HashMap<String, CachedCredential>>,
    refresh_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl OAuthCredentialStore {
    pub(crate) fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            secrets,
            cache: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn load(
        &self,
        profile: &StoredProfile,
        access: CredentialAccess,
    ) -> AppResult<CodexOAuthCredential> {
        validate_oauth_profile(profile)?;
        if let Some(cached) = self.cached(&profile.profile.id)? {
            if cached.persistence_pending && access == CredentialAccess::UserInitiated {
                self.persist(profile, &cached.credential, access).await?;
            }
            return Ok(cached.credential);
        }
        let reference = profile
            .secret_ref
            .as_deref()
            .ok_or(AppError::ProfileRuntimeUnavailable)?;
        let value = match access {
            CredentialAccess::Background => {
                self.secrets.get_without_user_interaction(reference).await
            }
            CredentialAccess::UserInitiated => self.secrets.get(reference).await,
        }
        .map_err(map_missing_credential)?;
        let credential = decode_credential(&value)?;
        self.cache(&profile.profile.id, credential.clone(), false)?;
        Ok(credential)
    }

    pub(crate) async fn current_or_refresh(
        &self,
        profile: &StoredProfile,
        access: CredentialAccess,
    ) -> AppResult<CodexOAuthCredential> {
        let credential = self.load(profile, access).await?;
        if !credential_needs_refresh(&credential) {
            return Ok(credential);
        }
        self.refresh(profile, access, false).await
    }

    pub(crate) async fn refresh(
        &self,
        profile: &StoredProfile,
        access: CredentialAccess,
        force: bool,
    ) -> AppResult<CodexOAuthCredential> {
        validate_oauth_profile(profile)?;
        let lock = self.refresh_lock(&profile.profile.id)?;
        let _guard = lock.lock().await;
        let credential = self.load(profile, access).await?;
        if !force && !credential_needs_refresh(&credential) {
            return Ok(credential);
        }
        let refreshed = refresh_credential(&credential).await?;
        self.persist(profile, &refreshed, access).await?;
        Ok(refreshed)
    }

    pub(crate) async fn persist(
        &self,
        profile: &StoredProfile,
        credential: &CodexOAuthCredential,
        access: CredentialAccess,
    ) -> AppResult<()> {
        if self
            .cached(&profile.profile.id)?
            .is_some_and(|cached| cached.credential == *credential && !cached.persistence_pending)
        {
            return Ok(());
        }
        let reference = profile
            .secret_ref
            .as_deref()
            .ok_or(AppError::ProfileRuntimeUnavailable)?;
        let encoded = serde_json::to_string(credential).map_err(|_| AppError::Internal)?;
        let result = match access {
            CredentialAccess::Background => {
                self.secrets
                    .set_without_user_interaction(reference, &encoded)
                    .await
            }
            CredentialAccess::UserInitiated => self.secrets.set(reference, &encoded).await,
        };
        match result {
            Ok(()) => self.cache(&profile.profile.id, credential.clone(), false),
            Err(AppError::KeychainInteractionRequired)
                if access == CredentialAccess::Background =>
            {
                self.cache(&profile.profile.id, credential.clone(), true)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn insert(
        &self,
        profile_id: &str,
        credential: CodexOAuthCredential,
        persistence_pending: bool,
    ) -> AppResult<()> {
        self.cache(profile_id, credential, persistence_pending)
    }

    pub(crate) fn remove(&self, profile_id: &str) -> AppResult<()> {
        self.cache
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(profile_id);
        self.refresh_locks
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(profile_id);
        Ok(())
    }

    fn cached(&self, profile_id: &str) -> AppResult<Option<CachedCredential>> {
        self.cache
            .lock()
            .map_err(|_| AppError::Internal)
            .map(|cache| cache.get(profile_id).cloned())
    }

    fn cache(
        &self,
        profile_id: &str,
        credential: CodexOAuthCredential,
        persistence_pending: bool,
    ) -> AppResult<()> {
        self.cache.lock().map_err(|_| AppError::Internal)?.insert(
            profile_id.to_owned(),
            CachedCredential {
                credential,
                persistence_pending,
            },
        );
        Ok(())
    }

    fn refresh_lock(&self, profile_id: &str) -> AppResult<Arc<AsyncMutex<()>>> {
        let mut locks = self.refresh_locks.lock().map_err(|_| AppError::Internal)?;
        Ok(locks
            .entry(profile_id.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone())
    }
}

fn validate_oauth_profile(profile: &StoredProfile) -> AppResult<()> {
    if profile.profile.kind != ProfileKind::CodexOauth
        || profile.profile.auth_mode != CodexAuthMode::OAuth
    {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    Ok(())
}

fn decode_credential(value: &str) -> AppResult<CodexOAuthCredential> {
    serde_json::from_str(value)
        .or_else(|_| {
            serde_json::from_str::<ImportedAuthFileCredential>(value)
                .ok()
                .filter(|credential| credential.auth_mode == CodexAuthMode::OAuth)
                .and_then(|credential| CodexOAuthCredential::from_auth_json(&credential.auth_json))
                .ok_or_else(|| serde_json::Error::io(std::io::Error::other("not oauth")))
        })
        .map_err(|_| AppError::ProfileRuntimeUnavailable)
}

fn map_missing_credential(error: AppError) -> AppError {
    match error {
        AppError::NotFound => AppError::ProfileRuntimeUnavailable,
        other => other,
    }
}

pub(crate) fn credential_needs_refresh(credential: &CodexOAuthCredential) -> bool {
    let payload = credential
        .access_token
        .split('.')
        .nth(1)
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .and_then(|value| serde_json::from_slice::<serde_json::Value>(&value).ok());
    payload
        .and_then(|value| value.get("exp").and_then(|value| value.as_i64()))
        .is_some_and(|expiry| expiry * 1000 <= timestamp_ms() + 300_000)
}

pub(crate) async fn refresh_credential(
    credential: &CodexOAuthCredential,
) -> AppResult<CodexOAuthCredential> {
    #[derive(Deserialize)]
    struct TokenResponse {
        id_token: Option<String>,
        access_token: String,
        refresh_token: Option<String>,
    }
    let refresh = credential
        .refresh_token
        .as_deref()
        .ok_or(AppError::ProfileRuntimeUnavailable)?;
    let response = reqwest::Client::new()
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", OAUTH_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|_| AppError::RuntimeUnavailable)?;
    if !response.status().is_success() {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    let token: TokenResponse = response
        .json()
        .await
        .map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(CodexOAuthCredential {
        id_token: token
            .id_token
            .unwrap_or_else(|| credential.id_token.clone()),
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .filter(|value| !value.is_empty())
            .or_else(|| credential.refresh_token.clone()),
        account_id: credential.account_id.clone(),
        last_refresh_ms: timestamp_ms(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{CredentialAccess, OAuthCredentialStore};
    use crate::{
        database::StoredProfile,
        domain::{GatewayProvider, GatewayWireApi, MaskedProfile, ProfileKind},
        profiles::{CodexOAuthCredential, ImportedAuthFileCredential},
        secrets::{MemorySecretStore, SecretStore},
    };

    fn profile() -> StoredProfile {
        StoredProfile {
            profile: MaskedProfile {
                id: "oauth".into(),
                alias: "OAuth".into(),
                kind: ProfileKind::CodexOauth,
                base_url: None,
                provider: GatewayProvider::OpenAi,
                wire_api: GatewayWireApi::Responses,
                enabled: true,
                in_pool: true,
                priority: 0,
                weight: 1,
                models: vec!["gpt-test".into()],
                model_mappings: Vec::new(),
                health: "healthy".into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
            },
            secret_ref: Some("profile:oauth:oauth".into()),
            credential_fingerprint: None,
        }
    }

    #[test]
    fn concurrent_refreshes_share_one_profile_lock() {
        let store = OAuthCredentialStore::new(Arc::new(MemorySecretStore::new()));
        let first = store.refresh_lock("oauth").unwrap();
        let second = store.refresh_lock("oauth").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn loads_oauth_from_an_imported_auth_json_wrapper() {
        let secrets = Arc::new(MemorySecretStore::new());
        let credential = CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: Some("account".into()),
            last_refresh_ms: 1,
        };
        let wrapped = ImportedAuthFileCredential {
            version: 1,
            auth_mode: Default::default(),
            auth_json: credential.auth_json().unwrap(),
        };
        secrets
            .set(
                "profile:oauth:oauth",
                &serde_json::to_string(&wrapped).unwrap(),
            )
            .await
            .unwrap();
        let store = OAuthCredentialStore::new(secrets);
        let loaded = store
            .load(&profile(), CredentialAccess::Background)
            .await
            .unwrap();
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.account_id.as_deref(), Some("account"));
    }
}
