use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::{stream, StreamExt};

use argon2::{
    password_hash::{rand_core::OsRng, SaltString},
    Argon2, PasswordHasher,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use tauri::State;
use uuid::Uuid;

use crate::{
    codex_runtime::{CodexRuntime, DesktopWorkspaceLaunch},
    database::Repository,
    domain::{
        CancelManagedTaskInput, CompleteOAuthImportInput, CreateClientKeyInput, CreateProfileInput,
        CreatedClientKey, CurrentProfileActivation, CurrentProfileActivationStatusInput,
        DashboardSnapshot, DeleteDesktopWorkspaceInput, DesktopWorkspaceHistoryItem,
        DesktopWorkspaceMode, DesktopWorkspaceSettings, GatewayStatus, ManagedTaskStatus,
        MaskedChannel, MaskedClientKey, MaskedProfile, OAuthImportStatus,
        ProfileQuotaRefreshReport, RestoreDesktopWorkspaceInput, SelectCurrentProfileInput,
        StartManagedTaskInput, StartOAuthImportInput, TestChannelInput,
        UpdateDesktopWorkspaceSettingsInput, UpdateGatewayInput, UpdateProfileInput,
        UpsertChannelInput,
    },
    error::{AppError, AppResult},
    gateway::{validate_binding, GatewayManager},
    notifications, profiles,
    secrets::SecretStore,
};

pub struct AppState {
    pub repository: Arc<Repository>,
    pub secrets: Arc<dyn SecretStore>,
    pub gateway: Arc<GatewayManager>,
    pub runtime: Arc<CodexRuntime>,
    pub quota_refresh_lock: tokio::sync::Mutex<()>,
    pub(crate) oauth_credentials: std::sync::Mutex<HashMap<String, CachedOAuthCredential>>,
}

#[derive(Clone)]
pub(crate) struct CachedOAuthCredential {
    credential: profiles::CodexOAuthCredential,
    persistence_pending: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OAuthCredentialAccess {
    Background,
    UserInitiated,
}

#[tauri::command]
pub fn dashboard_snapshot(state: State<'_, AppState>) -> AppResult<DashboardSnapshot> {
    Ok(DashboardSnapshot {
        gateway: state.gateway.status()?,
        profiles: state
            .repository
            .list_profiles()?
            .into_iter()
            .map(|stored| stored.profile)
            .collect(),
        metrics: state.repository.metrics()?,
        notifications: state
            .repository
            .list_channels()?
            .into_iter()
            .map(|stored| stored.channel)
            .collect(),
    })
}

#[tauri::command]
pub fn list_profiles(state: State<'_, AppState>) -> AppResult<Vec<MaskedProfile>> {
    Ok(state
        .repository
        .list_profiles()?
        .into_iter()
        .map(|stored| stored.profile)
        .collect())
}

#[tauri::command]
pub async fn create_profile(
    input: CreateProfileInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    profiles::create_profile(&state.repository, state.secrets.clone(), input).await
}

#[tauri::command]
pub async fn update_profile(
    input: UpdateProfileInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    profiles::update_profile(&state.repository, state.secrets.clone(), input).await
}

#[tauri::command]
pub async fn sync_profile_account_info(
    id: String,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    let _guard = state.quota_refresh_lock.lock().await;
    sync_profile_quota_or_mark_stale(&state, &id, OAuthCredentialAccess::UserInitiated).await
}

#[tauri::command]
pub async fn refresh_profile_quotas(
    state: State<'_, AppState>,
) -> AppResult<ProfileQuotaRefreshReport> {
    let _guard = state.quota_refresh_lock.lock().await;
    let profile_ids = state
        .repository
        .list_profiles()?
        .into_iter()
        .filter(|profile| {
            profile.profile.kind == crate::domain::ProfileKind::CodexOauth
                && profile.profile.credential_configured
        })
        .map(|profile| profile.profile.id)
        .collect::<Vec<_>>();
    let app_state = state.inner();
    let results = stream::iter(profile_ids.into_iter().map(|id| async move {
        let profile =
            sync_profile_quota_or_mark_stale(app_state, &id, OAuthCredentialAccess::Background)
                .await;
        (id, profile)
    }))
    .buffer_unordered(3)
    .collect::<Vec<_>>()
    .await;
    let mut profiles = Vec::with_capacity(results.len());
    let mut failed_profile_ids = Vec::new();
    for (id, result) in results {
        match result {
            Ok(profile) => {
                if profile
                    .account
                    .as_ref()
                    .is_some_and(|account| account.quota.status != "available")
                {
                    failed_profile_ids.push(id);
                }
                profiles.push(profile);
            }
            Err(_) => failed_profile_ids.push(id),
        }
    }
    Ok(ProfileQuotaRefreshReport {
        profiles,
        failed_profile_ids,
    })
}

async fn sync_profile_quota(
    state: &AppState,
    id: &str,
    access: OAuthCredentialAccess,
) -> AppResult<MaskedProfile> {
    let profile = state.repository.profile(id)?;
    let credential = oauth_credential(state, &profile, access).await?;
    match state.runtime.read_profile_rate_limits(id, credential).await {
        Ok(result) => {
            persist_oauth_credential(state, &profile, &result.credential, access).await?;
            profiles::save_oauth_credential_metadata(&state.repository, id, &result.credential)?;
            profiles::sync_oauth_account_info_with_snapshot(
                &state.repository,
                id,
                &result.credential,
                result.quota,
                result.subscription,
                result.email,
                result.account_id,
            )
        }
        Err(_) => profiles::mark_quota_stale(&state.repository, id),
    }
}

async fn sync_profile_quota_or_mark_stale(
    state: &AppState,
    id: &str,
    access: OAuthCredentialAccess,
) -> AppResult<MaskedProfile> {
    match sync_profile_quota(state, id, access).await {
        Ok(profile) => Ok(profile),
        Err(AppError::KeychainInteractionRequired)
            if access == OAuthCredentialAccess::Background =>
        {
            profiles::mark_quota_stale_for_keychain_interaction(&state.repository, id)
        }
        Err(error) => profiles::mark_quota_stale(&state.repository, id).or(Err(error)),
    }
}

#[tauri::command]
pub async fn delete_profile(
    id: String,
    confirmed: bool,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if !confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let profile = state.repository.profile(&id)?;
    if profile.profile.kind == crate::domain::ProfileKind::CodexOauth {
        state.runtime.logout_profile(&id)?;
    }
    profiles::delete_profile(&state.repository, state.secrets.clone(), &id, confirmed).await?;
    remove_cached_oauth_credential(state.inner(), &id)?;
    Ok(())
}

#[tauri::command]
pub async fn select_current_profile(
    input: SelectCurrentProfileInput,
    state: State<'_, AppState>,
) -> AppResult<CurrentProfileActivation> {
    let mode = state.repository.desktop_workspace_mode()?;
    if mode == DesktopWorkspaceMode::Shared && !input.confirmed_desktop_restart {
        return Err(AppError::ConfirmationRequired);
    }
    let profile = state.repository.profile(&input.id)?;
    let credential = oauth_credential(
        state.inner(),
        &profile,
        OAuthCredentialAccess::UserInitiated,
    )
    .await?;
    if mode == DesktopWorkspaceMode::Shared {
        state.runtime.quit_desktop_for_shared_switch()?;
    }
    let workspace = match mode {
        DesktopWorkspaceMode::Fresh => {
            let workspace_id = Uuid::new_v4().to_string();
            state
                .repository
                .create_desktop_workspace(&workspace_id, &input.id, now_ms())?;
            DesktopWorkspaceLaunch::Fresh(workspace_id)
        }
        DesktopWorkspaceMode::PerProfile => DesktopWorkspaceLaunch::PerProfile,
        DesktopWorkspaceMode::Shared => DesktopWorkspaceLaunch::Shared,
    };
    let activation = state
        .runtime
        .activate_profile(profile, credential, workspace)?;
    Ok(activation)
}

#[tauri::command]
pub fn desktop_workspace_settings(
    state: State<'_, AppState>,
) -> AppResult<DesktopWorkspaceSettings> {
    Ok(DesktopWorkspaceSettings {
        mode: state.repository.desktop_workspace_mode()?,
    })
}

#[tauri::command]
pub fn update_desktop_workspace_settings(
    input: UpdateDesktopWorkspaceSettingsInput,
    state: State<'_, AppState>,
) -> AppResult<DesktopWorkspaceSettings> {
    state.repository.set_desktop_workspace_mode(&input.mode)?;
    Ok(DesktopWorkspaceSettings { mode: input.mode })
}

#[tauri::command]
pub fn list_desktop_workspaces(
    state: State<'_, AppState>,
) -> AppResult<Vec<DesktopWorkspaceHistoryItem>> {
    state.repository.list_desktop_workspaces()
}

#[tauri::command]
pub async fn restore_desktop_workspace(
    input: RestoreDesktopWorkspaceInput,
    state: State<'_, AppState>,
) -> AppResult<CurrentProfileActivation> {
    let workspace = state.repository.desktop_workspace(&input.id)?;
    let profile = state.repository.profile(&workspace.profile_id)?;
    let credential = oauth_credential(
        state.inner(),
        &profile,
        OAuthCredentialAccess::UserInitiated,
    )
    .await?;
    state
        .repository
        .touch_desktop_workspace(&workspace.id, now_ms())?;
    state.runtime.activate_profile(
        profile,
        credential,
        DesktopWorkspaceLaunch::Fresh(workspace.id),
    )
}

#[tauri::command]
pub fn delete_desktop_workspace(
    input: DeleteDesktopWorkspaceInput,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let workspace = state.repository.desktop_workspace(&input.id)?;
    state
        .runtime
        .delete_fresh_desktop_workspace(&workspace.id)?;
    state.repository.delete_desktop_workspace(&workspace.id)
}

#[tauri::command]
pub fn current_profile_activation_status(
    input: CurrentProfileActivationStatusInput,
    state: State<'_, AppState>,
) -> AppResult<CurrentProfileActivation> {
    let activation = state
        .runtime
        .current_profile_activation_status(&input.attempt_id)?;
    persist_current_profile_if_activated(&state.repository, &activation)?;
    Ok(activation)
}

fn persist_current_profile_if_activated(
    repository: &Repository,
    activation: &CurrentProfileActivation,
) -> AppResult<()> {
    if matches!(
        activation.status.as_str(),
        "activated" | "desktop_restart_failed"
    ) {
        profiles::select_current_profile(repository, &activation.profile_id)?;
    }
    Ok(())
}

#[tauri::command]
pub fn managed_task_status(state: State<'_, AppState>) -> AppResult<ManagedTaskStatus> {
    state.runtime.managed_task_status()
}

#[tauri::command]
pub async fn start_managed_task(
    input: StartManagedTaskInput,
    state: State<'_, AppState>,
) -> AppResult<ManagedTaskStatus> {
    let profile = state
        .repository
        .list_profiles()?
        .into_iter()
        .find(|stored| stored.profile.is_current)
        .ok_or(AppError::CurrentProfileRequired)?;
    let credential = oauth_credential(
        state.inner(),
        &profile,
        OAuthCredentialAccess::UserInitiated,
    )
    .await?;
    state
        .runtime
        .start_managed_task(&profile, credential, input)
}

#[tauri::command]
pub fn cancel_managed_task(
    input: CancelManagedTaskInput,
    state: State<'_, AppState>,
) -> AppResult<ManagedTaskStatus> {
    state.runtime.cancel_managed_task(input.confirmed)
}

#[tauri::command]
pub fn gateway_status(state: State<'_, AppState>) -> AppResult<GatewayStatus> {
    state.gateway.status()
}

#[tauri::command]
pub fn update_gateway(
    input: UpdateGatewayInput,
    state: State<'_, AppState>,
) -> AppResult<GatewayStatus> {
    if input.bind_mode == "lan" && !input.confirmed_lan {
        return Err(AppError::ConfirmationRequired);
    }
    let address = input
        .bind_address
        .parse()
        .map_err(|_| AppError::ValidationFailed)?;
    validate_binding(&input.bind_mode, address, &input.cidrs)?;
    if state.gateway.is_running() {
        return Err(AppError::Conflict);
    }
    state.repository.update_gateway_settings(
        &input.bind_mode,
        &input.bind_address,
        input.port,
        &input.cidrs,
    )?;
    state.gateway.status()
}

#[tauri::command]
pub async fn start_gateway(state: State<'_, AppState>) -> AppResult<GatewayStatus> {
    state.gateway.start().await
}

#[tauri::command]
pub fn stop_gateway(state: State<'_, AppState>) -> AppResult<GatewayStatus> {
    state.gateway.stop()
}

#[tauri::command]
pub fn list_client_keys(state: State<'_, AppState>) -> AppResult<Vec<MaskedClientKey>> {
    state.repository.list_client_keys()
}

#[tauri::command]
pub async fn create_client_key(
    input: CreateClientKeyInput,
    state: State<'_, AppState>,
) -> AppResult<CreatedClientKey> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    if input.name.trim().is_empty() {
        return Err(AppError::ValidationFailed);
    }
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
    state.secrets.set(&secret_ref, &plaintext_once).await?;
    let key = MaskedClientKey {
        id,
        name: input.name.trim().to_owned(),
        masked_value,
        created_at_ms: now_ms(),
        last_used_at_ms: None,
        revoked: false,
    };
    if let Err(error) = state.repository.insert_client_key(&key, &hash, &secret_ref) {
        let _ = state.secrets.delete(&secret_ref).await;
        return Err(error);
    }
    Ok(CreatedClientKey {
        key,
        plaintext_once,
    })
}

#[tauri::command]
pub async fn revoke_client_key(
    id: String,
    confirmed: bool,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if !confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    if let Some(reference) = state.repository.revoke_client_key(&id)? {
        state.secrets.delete(&reference).await?;
    }
    Ok(())
}

#[tauri::command]
pub fn list_channels(state: State<'_, AppState>) -> AppResult<Vec<MaskedChannel>> {
    Ok(state
        .repository
        .list_channels()?
        .into_iter()
        .map(|stored| stored.channel)
        .collect())
}

#[tauri::command]
pub async fn upsert_channel(
    input: UpsertChannelInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedChannel> {
    notifications::upsert_channel(&state.repository, state.secrets.clone(), input).await
}

#[tauri::command]
pub async fn test_channel(
    input: TestChannelInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedChannel> {
    notifications::test_channel(&state.repository, state.secrets.clone(), input).await
}

#[tauri::command]
pub async fn delete_channel(
    id: String,
    confirmed: bool,
    state: State<'_, AppState>,
) -> AppResult<()> {
    notifications::delete_channel(&state.repository, state.secrets.clone(), &id, confirmed).await
}

#[tauri::command]
pub fn start_oauth_import(
    input: StartOAuthImportInput,
    state: State<'_, AppState>,
) -> AppResult<OAuthImportStatus> {
    if let Some(profile_id) = input.profile_id.as_deref() {
        let profile = state.repository.profile(profile_id)?;
        if profile.profile.kind != crate::domain::ProfileKind::CodexOauth {
            return Err(AppError::ValidationFailed);
        }
        remove_cached_oauth_credential(state.inner(), profile_id)?;
    }
    state.runtime.start_oauth_import(input.profile_id)
}

#[tauri::command]
pub fn oauth_import_status(
    attempt_id: String,
    state: State<'_, AppState>,
) -> AppResult<OAuthImportStatus> {
    state.runtime.oauth_import_status(&attempt_id)
}

#[tauri::command]
pub fn cancel_oauth_import(attempt_id: String, state: State<'_, AppState>) -> AppResult<()> {
    state.runtime.cancel_oauth_import(&attempt_id)
}

#[tauri::command]
pub async fn complete_oauth_import(
    input: CompleteOAuthImportInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    let completed = state.runtime.complete_oauth_import(&input.attempt_id)?;
    let profile = match completed.profile_id {
        Some(profile_id) => {
            profiles::save_oauth_credential(
                &state.repository,
                state.secrets.clone(),
                &profile_id,
                &completed.credential,
            )
            .await
        }
        None => {
            profiles::create_oauth_profile(
                &state.repository,
                state.secrets.clone(),
                input.attempt_id,
                input.alias.ok_or(AppError::ValidationFailed)?,
                &completed.credential,
            )
            .await
        }
    }?;
    cache_oauth_credential(
        state.inner(),
        &profile.id,
        completed.credential.clone(),
        false,
    )?;
    match state
        .runtime
        .read_profile_rate_limits(&profile.id, completed.credential)
        .await
    {
        Ok(result) => {
            let stored = state.repository.profile(&profile.id)?;
            persist_oauth_credential(
                state.inner(),
                &stored,
                &result.credential,
                OAuthCredentialAccess::UserInitiated,
            )
            .await?;
            profiles::save_oauth_credential_metadata(
                &state.repository,
                &profile.id,
                &result.credential,
            )?;
            profiles::sync_oauth_account_info_with_snapshot(
                &state.repository,
                &profile.id,
                &result.credential,
                result.quota,
                result.subscription,
                result.email,
                result.account_id,
            )
        }
        Err(_) => profiles::mark_quota_stale(&state.repository, &profile.id),
    }
}

async fn oauth_credential(
    state: &AppState,
    profile: &crate::database::StoredProfile,
    access: OAuthCredentialAccess,
) -> AppResult<crate::profiles::CodexOAuthCredential> {
    if profile.profile.kind != crate::domain::ProfileKind::CodexOauth {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    let reference = profile
        .secret_ref
        .as_deref()
        .ok_or(AppError::ProfileRuntimeUnavailable)?;
    if let Some(cached) = cached_oauth_credential(state, &profile.profile.id)? {
        if cached.persistence_pending && access == OAuthCredentialAccess::UserInitiated {
            persist_oauth_credential(state, profile, &cached.credential, access).await?;
        }
        return Ok(cached.credential);
    }
    let value = match access {
        OAuthCredentialAccess::Background => {
            state.secrets.get_without_user_interaction(reference).await
        }
        OAuthCredentialAccess::UserInitiated => state.secrets.get(reference).await,
    }
    .map_err(|error| match error {
        AppError::NotFound => AppError::ProfileRuntimeUnavailable,
        other => other,
    })?;
    let credential: crate::profiles::CodexOAuthCredential =
        serde_json::from_str(&value).map_err(|_| AppError::ProfileRuntimeUnavailable)?;
    cache_oauth_credential(state, &profile.profile.id, credential.clone(), false)?;
    Ok(credential)
}

async fn persist_oauth_credential(
    state: &AppState,
    profile: &crate::database::StoredProfile,
    credential: &crate::profiles::CodexOAuthCredential,
    access: OAuthCredentialAccess,
) -> AppResult<()> {
    let previous = cached_oauth_credential(state, &profile.profile.id)?;
    if previous
        .as_ref()
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
        OAuthCredentialAccess::Background => {
            state
                .secrets
                .set_without_user_interaction(reference, &encoded)
                .await
        }
        OAuthCredentialAccess::UserInitiated => state.secrets.set(reference, &encoded).await,
    };
    match result {
        Ok(()) => cache_oauth_credential(state, &profile.profile.id, credential.clone(), false),
        Err(AppError::KeychainInteractionRequired)
            if access == OAuthCredentialAccess::Background =>
        {
            cache_oauth_credential(state, &profile.profile.id, credential.clone(), true)
        }
        Err(error) => Err(error),
    }
}

fn cached_oauth_credential(
    state: &AppState,
    profile_id: &str,
) -> AppResult<Option<CachedOAuthCredential>> {
    state
        .oauth_credentials
        .lock()
        .map_err(|_| AppError::Internal)
        .map(|cache| cache.get(profile_id).cloned())
}

fn cache_oauth_credential(
    state: &AppState,
    profile_id: &str,
    credential: crate::profiles::CodexOAuthCredential,
    persistence_pending: bool,
) -> AppResult<()> {
    state
        .oauth_credentials
        .lock()
        .map_err(|_| AppError::Internal)?
        .insert(
            profile_id.to_owned(),
            CachedOAuthCredential {
                credential,
                persistence_pending,
            },
        );
    Ok(())
}

fn remove_cached_oauth_credential(state: &AppState, profile_id: &str) -> AppResult<()> {
    state
        .oauth_credentials
        .lock()
        .map_err(|_| AppError::Internal)?
        .remove(profile_id);
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    fn activation(status: &str) -> CurrentProfileActivation {
        CurrentProfileActivation {
            profile_id: "oauth-profile".to_owned(),
            attempt_id: Some("attempt".to_owned()),
            status: status.to_owned(),
            message: "非敏感状态".to_owned(),
        }
    }

    fn test_app_state(repository: Arc<Repository>, secrets: Arc<dyn SecretStore>) -> AppState {
        AppState {
            gateway: Arc::new(GatewayManager::new(
                repository.clone(),
                secrets.clone(),
                PathBuf::from("/tmp/codex-relay-test-certs"),
            )),
            runtime: Arc::new(CodexRuntime::new(
                PathBuf::from("/tmp/codex-relay-test-runtime"),
                secrets.clone(),
            )),
            repository,
            secrets,
            quota_refresh_lock: tokio::sync::Mutex::new(()),
            oauth_credentials: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[derive(Default)]
    struct TrackingSecretStore {
        values: std::sync::Mutex<HashMap<String, String>>,
        interactive_gets: AtomicUsize,
        interactive_sets: AtomicUsize,
        silent_gets: AtomicUsize,
        silent_sets: AtomicUsize,
        silent_access_denied: AtomicBool,
    }

    impl TrackingSecretStore {
        fn reset_counts(&self) {
            self.interactive_gets.store(0, Ordering::Relaxed);
            self.interactive_sets.store(0, Ordering::Relaxed);
            self.silent_gets.store(0, Ordering::Relaxed);
            self.silent_sets.store(0, Ordering::Relaxed);
        }

        fn read(&self, reference: &str) -> AppResult<String> {
            self.values
                .lock()
                .map_err(|_| AppError::Internal)?
                .get(reference)
                .cloned()
                .ok_or(AppError::NotFound)
        }

        fn write(&self, reference: &str, value: &str) -> AppResult<()> {
            self.values
                .lock()
                .map_err(|_| AppError::Internal)?
                .insert(reference.to_owned(), value.to_owned());
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl SecretStore for TrackingSecretStore {
        async fn set(&self, reference: &str, value: &str) -> AppResult<()> {
            self.interactive_sets.fetch_add(1, Ordering::Relaxed);
            self.write(reference, value)
        }

        async fn get(&self, reference: &str) -> AppResult<String> {
            self.interactive_gets.fetch_add(1, Ordering::Relaxed);
            self.read(reference)
        }

        async fn set_without_user_interaction(
            &self,
            reference: &str,
            value: &str,
        ) -> AppResult<()> {
            self.silent_sets.fetch_add(1, Ordering::Relaxed);
            if self.silent_access_denied.load(Ordering::Relaxed) {
                return Err(AppError::KeychainInteractionRequired);
            }
            self.write(reference, value)
        }

        async fn get_without_user_interaction(&self, reference: &str) -> AppResult<String> {
            self.silent_gets.fetch_add(1, Ordering::Relaxed);
            if self.silent_access_denied.load(Ordering::Relaxed) {
                return Err(AppError::KeychainInteractionRequired);
            }
            self.read(reference)
        }

        async fn delete(&self, reference: &str) -> AppResult<()> {
            self.values
                .lock()
                .map_err(|_| AppError::Internal)?
                .remove(reference);
            Ok(())
        }
    }

    fn oauth_credential_value() -> crate::profiles::CodexOAuthCredential {
        crate::profiles::CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: None,
            last_refresh_ms: 1,
        }
    }

    #[tokio::test]
    async fn persists_the_current_profile_after_credential_projection() {
        let repository = Repository::memory();
        profiles::create_oauth_profile(
            &repository,
            Arc::new(crate::secrets::MemorySecretStore::new()),
            "oauth-profile".to_owned(),
            "OAuth profile".to_owned(),
            &crate::profiles::CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();

        persist_current_profile_if_activated(&repository, &activation("failed")).unwrap();
        assert!(
            !repository
                .profile("oauth-profile")
                .unwrap()
                .profile
                .is_current
        );

        persist_current_profile_if_activated(&repository, &activation("activated")).unwrap();
        assert!(
            repository
                .profile("oauth-profile")
                .unwrap()
                .profile
                .is_current
        );

        profiles::select_current_profile(&repository, "oauth-profile").unwrap();
        persist_current_profile_if_activated(&repository, &activation("desktop_restart_failed"))
            .unwrap();
        assert!(
            repository
                .profile("oauth-profile")
                .unwrap()
                .profile
                .is_current
        );
    }

    #[tokio::test]
    async fn treats_a_missing_oauth_credential_as_reauthorization_required() {
        let repository = Arc::new(Repository::memory());
        let saved_store = Arc::new(crate::secrets::MemorySecretStore::new());
        profiles::create_oauth_profile(
            &repository,
            saved_store,
            "oauth-profile".to_owned(),
            "OAuth profile".to_owned(),
            &crate::profiles::CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();

        let state = test_app_state(
            repository.clone(),
            Arc::new(crate::secrets::MemorySecretStore::new()),
        );
        let error = oauth_credential(
            &state,
            &repository.profile("oauth-profile").unwrap(),
            OAuthCredentialAccess::UserInitiated,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AppError::ProfileRuntimeUnavailable));
    }

    #[tokio::test]
    async fn background_credential_reads_are_silent_and_cached_for_the_session() {
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(TrackingSecretStore::default());
        let credential = oauth_credential_value();
        profiles::create_oauth_profile(
            &repository,
            secrets.clone(),
            "oauth-profile".into(),
            "OAuth profile".into(),
            &credential,
        )
        .await
        .unwrap();
        secrets.reset_counts();
        let state = test_app_state(repository.clone(), secrets.clone());
        let profile = repository.profile("oauth-profile").unwrap();

        assert_eq!(
            oauth_credential(&state, &profile, OAuthCredentialAccess::Background)
                .await
                .unwrap(),
            credential
        );
        assert!(
            oauth_credential(&state, &profile, OAuthCredentialAccess::Background)
                .await
                .is_ok()
        );

        assert_eq!(secrets.silent_gets.load(Ordering::Relaxed), 1);
        assert_eq!(secrets.interactive_gets.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn background_credential_reads_never_fall_back_to_an_interactive_prompt() {
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(TrackingSecretStore::default());
        profiles::create_oauth_profile(
            &repository,
            secrets.clone(),
            "oauth-profile".into(),
            "OAuth profile".into(),
            &oauth_credential_value(),
        )
        .await
        .unwrap();
        secrets.reset_counts();
        secrets.silent_access_denied.store(true, Ordering::Relaxed);
        let state = test_app_state(repository.clone(), secrets.clone());

        let error = oauth_credential(
            &state,
            &repository.profile("oauth-profile").unwrap(),
            OAuthCredentialAccess::Background,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AppError::KeychainInteractionRequired));
        assert_eq!(secrets.silent_gets.load(Ordering::Relaxed), 1);
        assert_eq!(secrets.interactive_gets.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn user_action_flushes_a_pending_background_credential_write() {
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(TrackingSecretStore::default());
        let credential = oauth_credential_value();
        profiles::create_oauth_profile(
            &repository,
            secrets.clone(),
            "oauth-profile".into(),
            "OAuth profile".into(),
            &credential,
        )
        .await
        .unwrap();
        secrets.reset_counts();
        let state = test_app_state(repository.clone(), secrets.clone());
        cache_oauth_credential(&state, "oauth-profile", credential, true).unwrap();

        assert!(oauth_credential(
            &state,
            &repository.profile("oauth-profile").unwrap(),
            OAuthCredentialAccess::UserInitiated,
        )
        .await
        .is_ok());

        assert_eq!(secrets.interactive_sets.load(Ordering::Relaxed), 1);
        assert!(cached_oauth_credential(&state, "oauth-profile")
            .unwrap()
            .is_some_and(|cached| !cached.persistence_pending));
    }
}
