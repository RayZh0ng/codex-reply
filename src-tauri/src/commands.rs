use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::collections::HashMap;

use futures_util::{stream, StreamExt};

use argon2::{
    password_hash::{rand_core::OsRng, SaltString},
    Argon2, PasswordHasher,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use uuid::Uuid;

use crate::{
    codex_environment, codex_gateway,
    codex_runtime::{CodexRuntime, DesktopWorkspaceLaunch},
    codex_session_history,
    collaboration::CollaborationManager,
    database::Repository,
    domain::{
        ApiServiceProfileUpdateResult, AppUpdateChannel, AppUpdateInfo, AppUpdateProgressEvent,
        AppUpdateProgressPhase, AppUpdateSettings, CancelCodexSessionInput, CancelManagedTaskInput,
        CheckAppUpdateInput, ClientKeySecretInput, CodexAuthMode, CodexEnvironmentInstallReport,
        CodexEnvironmentReport, CodexHistoryExportReport, CodexHistoryImportReport,
        CodexHistoryMutationReport, CodexHistoryReport, CodexHistorySyncReport,
        CodexHistoryTransitionStatus, CodexSessionSummary, CollaborationCallbackStatus,
        CollaborationContextSummary, CollaborationProjectBinding, CollaborationProvider,
        CommitJsonProfileImportInput, CompleteOAuthImportInput, ContinueCodexSessionInput,
        CreateApiServiceProfileInput, CreateClientKeyInput, CreateProfileInput, CreatedClientKey,
        CurrentProfileActivation, CurrentProfileActivationStatusInput, DashboardSnapshot,
        DeleteCodexHistoryInput, DeleteCollaborationBotInput,
        DeleteCollaborationProjectBindingInput, DeleteDesktopWorkspaceInput, DeleteFeishuBotInput,
        DeleteFeishuProjectBindingInput, DesktopWorkspaceHistoryItem, DesktopWorkspaceSettings,
        DiscardJsonProfileImportInput, ExportCodexHistoryInput, FeishuProjectBinding,
        GatewayCodexConfigStatus, GatewayProvider, GatewayStatus, GatewayWireApi,
        ImportCodexHistoryInput, InstallAppUpdateInput, InstallCodexEnvironmentInput,
        JsonProfileImportPreview, JsonProfileImportResult, ListCodexHistoryInput,
        ListCodexSessionsInput, ManagedTaskStatus, MaskedClientKey, MaskedCollaborationBot,
        MaskedFeishuBot, MaskedProfile, OAuthImportStatus, PreviewJsonProfileImportInput,
        ProfileQuotaRefreshReport, ResetCollaborationContextInput, RestoreDesktopWorkspaceInput,
        RetryJsonProfileImportInput, SelectCurrentProfileInput, SetCodexGatewayOAuthProfileInput,
        StartManagedTaskInput, StartOAuthImportInput, SyncCodexHistoryInput, TestApiServiceInput,
        UpdateAppUpdateSettingsInput, UpdateCollaborationContextInput,
        UpdateDesktopWorkspaceSettingsInput, UpdateGatewayInput, UpdateProfileInput,
        UpsertCollaborationBotInput, UpsertCollaborationProjectBindingInput, UpsertFeishuBotInput,
        UpsertFeishuProjectBindingInput, APP_UPDATE_PROGRESS_EVENT,
        CODEX_HISTORY_SYNC_FINISHED_EVENT,
    },
    error::{AppError, AppResult},
    gateway::{
        available_lan_addresses, configure_gateway_upstream_proxy, discover_profile_models,
        test_api_service, validate_binding, GatewayManager,
    },
    oauth_credentials::{CredentialAccess as OAuthCredentialAccess, OAuthCredentialStore},
    profiles,
    secrets::SecretStore,
};

pub struct AppState {
    pub repository: Arc<Repository>,
    pub data_dir: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    pub gateway: Arc<GatewayManager>,
    pub runtime: Arc<CodexRuntime>,
    pub collaboration: Arc<CollaborationManager>,
    pub quota_refresh_lock: tokio::sync::Mutex<()>,
    pub history_sync_lock: Arc<tokio::sync::Mutex<()>>,
    pub history_transition_status: Arc<Mutex<Option<CodexHistoryTransitionStatus>>>,
    pub(crate) oauth_credentials: Arc<OAuthCredentialStore>,
    pub json_imports: crate::profile_import::JsonProfileImportStore,
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
        workspace_mode: state.repository.desktop_workspace_mode()?,
        collaboration: state.repository.collaboration_summary()?,
    })
}

#[tauri::command]
pub async fn list_codex_history(
    input: Option<ListCodexHistoryInput>,
    state: State<'_, AppState>,
) -> AppResult<CodexHistoryReport> {
    let repository = Arc::clone(&state.repository);
    let data_dir = state.data_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        codex_session_history::list_codex_history(
            repository.as_ref(),
            &data_dir,
            input.unwrap_or_default(),
        )
    })
    .await
    .map_err(|_| AppError::Internal)?
}

#[tauri::command]
pub async fn sync_codex_history(
    input: SyncCodexHistoryInput,
    state: State<'_, AppState>,
) -> AppResult<CodexHistorySyncReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    sync_codex_history_locked(state.inner(), None).await
}

#[tauri::command]
pub async fn delete_codex_history(
    input: DeleteCodexHistoryInput,
    state: State<'_, AppState>,
) -> AppResult<CodexHistoryMutationReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let _guard = state.history_sync_lock.lock().await;
    let repository = Arc::clone(&state.repository);
    let data_dir = state.data_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        codex_session_history::delete_codex_history(repository.as_ref(), &data_dir, input)
    })
    .await
    .map_err(|_| AppError::Internal)?
}

#[tauri::command]
pub async fn export_codex_history(
    input: ExportCodexHistoryInput,
    state: State<'_, AppState>,
) -> AppResult<CodexHistoryExportReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let _guard = state.history_sync_lock.lock().await;
    let repository = Arc::clone(&state.repository);
    let data_dir = state.data_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        codex_session_history::export_codex_history(repository.as_ref(), &data_dir, input)
    })
    .await
    .map_err(|_| AppError::Internal)?
}

#[tauri::command]
pub async fn import_codex_history(
    input: ImportCodexHistoryInput,
    state: State<'_, AppState>,
) -> AppResult<CodexHistoryImportReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let _guard = state.history_sync_lock.lock().await;
    let repository = Arc::clone(&state.repository);
    let data_dir = state.data_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        codex_session_history::import_codex_history(repository.as_ref(), &data_dir, input)
    })
    .await
    .map_err(|_| AppError::Internal)?
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
pub async fn create_api_service_profile(
    input: CreateApiServiceProfileInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    let report = test_api_service(&input.provider, &input.base_url, &input.api_key).await?;
    if report.status != "verified" {
        return Err(AppError::UpstreamUnavailable);
    }
    profiles::create_api_service_profile(
        &state.repository,
        state.secrets.clone(),
        input,
        report.models,
    )
    .await
}

#[tauri::command]
pub async fn update_profile(
    input: UpdateProfileInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    profiles::update_profile(&state.repository, state.secrets.clone(), input).await
}

#[tauri::command]
pub async fn update_api_service_profile(
    input: UpdateProfileInput,
    state: State<'_, AppState>,
) -> AppResult<ApiServiceProfileUpdateResult> {
    let profile_id = input.id.clone();
    let current_codex_config = codex_gateway::status(&state.repository).await?;
    let is_active_direct_profile = current_codex_config.enabled
        && current_codex_config.mode == "third_party"
        && current_codex_config.direct_profile_id.as_deref() == Some(profile_id.as_str());
    let oauth_profile_id = if is_active_direct_profile {
        match input.codex_oauth_profile_id.as_ref() {
            Some(Some(profile_id)) => Some(profile_id.clone()),
            Some(None) => None,
            None => {
                state
                    .repository
                    .profile(&profile_id)?
                    .profile
                    .codex_oauth_profile_id
            }
        }
    } else {
        None
    };
    if is_active_direct_profile {
        validate_oauth_profile_for_switch(state.inner(), oauth_profile_id.as_deref()).await?;
    }

    let profile = profiles::update_profile(&state.repository, state.secrets.clone(), input).await?;
    let codex_config = if is_active_direct_profile {
        let mut status = codex_gateway::sync_active_direct_oauth_profile(
            &state.repository,
            &state.oauth_credentials,
            &profile_id,
        )
        .await?;
        if profile.codex_oauth_profile_id.is_some() {
            append_shared_desktop_restart_result(state.inner(), &mut status);
        }
        Some(status)
    } else {
        None
    };
    Ok(ApiServiceProfileUpdateResult {
        profile,
        codex_config,
    })
}

#[tauri::command]
pub async fn preview_json_profile_import(
    input: PreviewJsonProfileImportInput,
    state: State<'_, AppState>,
) -> AppResult<JsonProfileImportPreview> {
    state
        .json_imports
        .preview(input, &state.repository, &state.runtime)
        .await
}

#[tauri::command]
pub async fn commit_json_profile_import(
    input: CommitJsonProfileImportInput,
    state: State<'_, AppState>,
) -> AppResult<JsonProfileImportResult> {
    state
        .json_imports
        .commit(input, &state.repository, state.secrets.clone())
        .await
}

#[tauri::command]
pub async fn retry_json_profile_import(
    input: RetryJsonProfileImportInput,
    state: State<'_, AppState>,
) -> AppResult<JsonProfileImportPreview> {
    state.json_imports.retry(input, &state.runtime).await
}

#[tauri::command]
pub fn discard_json_profile_import(
    input: DiscardJsonProfileImportInput,
    state: State<'_, AppState>,
) -> AppResult<()> {
    state.json_imports.discard(input)
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
    match profile.profile.auth_mode.clone() {
        CodexAuthMode::OAuth => {
            let credential = oauth_credential(state, &profile, access).await?;
            match state.runtime.read_profile_rate_limits(id, credential).await {
                Ok(result) => {
                    persist_oauth_credential(state, &profile, &result.credential, access).await?;
                    profiles::save_oauth_credential_metadata(
                        &state.repository,
                        id,
                        &result.credential,
                    )?;
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
                Err(error) => Err(error),
            }
        }
        CodexAuthMode::AgentIdentity | CodexAuthMode::PersonalAccessToken => {
            let auth_json = imported_auth_json(state, &profile, access).await?;
            match state
                .runtime
                .read_profile_rate_limits_auth_json(id, &auth_json)
                .await
            {
                Ok(result) => profiles::sync_codex_account_info_with_snapshot(
                    &state.repository,
                    id,
                    result.quota,
                    result.subscription,
                    result.email,
                    result.account_id,
                ),
                Err(error) => Err(error),
            }
        }
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
            profiles::mark_quota_stale_for_keychain_interaction(&state.repository, id)?;
            profiles::mark_profile_validation_unknown(
                &state.repository,
                id,
                "后台未读取系统钥匙串；档案有效性暂未确认。",
            )
        }
        Err(AppError::ProfileRuntimeUnavailable) => {
            profiles::mark_quota_stale(&state.repository, id)?;
            profiles::mark_profile_validation_invalid(
                &state.repository,
                id,
                "官方 Codex 接口拒绝了当前登录凭据，请重新授权。",
            )
        }
        Err(error) => {
            profiles::mark_quota_stale(&state.repository, id)?;
            profiles::mark_profile_validation_unknown(
                &state.repository,
                id,
                format!("档案有效性暂未确认：{error}"),
            )
        }
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
    app: AppHandle,
) -> AppResult<CurrentProfileActivation> {
    let workspace = desktop_workspace_for_current_profile_selection(&input)?;
    let profile = state.repository.profile(&input.id)?;
    let profile_id = profile.profile.id.clone();
    let target_home = state.data_dir.join("runtimes").join(&profile_id);
    let auth_before = auth_json_fingerprint(&target_home);
    let credential = active_profile_credential(state.inner(), &profile).await?;
    codex_gateway::restore_official_config(&state.repository, state.secrets.clone()).await?;
    state.runtime.quit_desktop_for_shared_switch()?;
    let mut activation = match credential {
        ActiveProfileCredential::OAuth(credential) => state
            .runtime
            .activate_profile(profile, credential, workspace)?,
        ActiveProfileCredential::AuthJson(auth_json) => state
            .runtime
            .activate_profile_auth_json(profile, auth_json, workspace)?,
    };
    let history_sync_status =
        queue_codex_history_transition_sync(state.inner(), &app, Some(target_home), auth_before);
    activation.message = append_history_transition_status(activation.message, &history_sync_status);
    activation.history_sync_status = Some(history_sync_status);
    Ok(activation)
}

fn desktop_workspace_for_current_profile_selection(
    input: &SelectCurrentProfileInput,
) -> AppResult<DesktopWorkspaceLaunch> {
    if !input.confirmed_desktop_restart {
        return Err(AppError::ConfirmationRequired);
    }
    Ok(DesktopWorkspaceLaunch::Shared)
}

async fn sync_codex_history_locked(
    state: &AppState,
    priority_home: Option<PathBuf>,
) -> AppResult<CodexHistorySyncReport> {
    let _guard = state.history_sync_lock.lock().await;
    let repository = Arc::clone(&state.repository);
    let data_dir = state.data_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if priority_home.is_some() {
            codex_session_history::sync_codex_history_prioritized_home(
                repository.as_ref(),
                &data_dir,
                priority_home.as_deref(),
            )
        } else {
            codex_session_history::sync_codex_history(repository.as_ref(), &data_dir)
        }
    })
    .await
    .map_err(|_| AppError::Internal)?
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthFileFingerprint {
    modified_at_ms: Option<i64>,
    len: u64,
}

fn queue_codex_history_transition_sync(
    state: &AppState,
    app: &AppHandle,
    target_home: Option<PathBuf>,
    target_auth_before: Option<AuthFileFingerprint>,
) -> CodexHistoryTransitionStatus {
    let queued_at_ms = now_ms();
    let queued = CodexHistoryTransitionStatus {
        status: "queued".to_owned(),
        message: "Codex 会话历史已排队后台恢复，切换不会等待完整扫描。".to_owned(),
        queued_at_ms,
        completed_at_ms: None,
        warnings: Vec::new(),
    };
    set_history_transition_status(state, queued.clone());

    let repository = Arc::clone(&state.repository);
    let data_dir = state.data_dir.clone();
    let history_sync_lock = Arc::clone(&state.history_sync_lock);
    let transition_status = Arc::clone(&state.history_transition_status);
    let app = app.clone();
    let priority_home = target_home.clone();

    tauri::async_runtime::spawn(async move {
        set_history_transition_status_arc(
            &transition_status,
            CodexHistoryTransitionStatus {
                status: "running".to_owned(),
                message: "Codex 会话历史正在后台恢复。".to_owned(),
                queued_at_ms,
                completed_at_ms: None,
                warnings: Vec::new(),
            },
        );

        let mut wait_warnings = Vec::new();
        if let Some(home) = target_home.as_deref() {
            wait_for_target_auth_json(home, target_auth_before, queued_at_ms, &mut wait_warnings)
                .await;
        }

        let result = {
            let _guard = history_sync_lock.lock().await;
            let repository = Arc::clone(&repository);
            let data_dir = data_dir.clone();
            let priority_home = priority_home.clone();
            tauri::async_runtime::spawn_blocking(move || {
                codex_session_history::sync_codex_history_prioritized_home(
                    repository.as_ref(),
                    &data_dir,
                    priority_home.as_deref(),
                )
            })
            .await
            .map_err(|_| AppError::Internal)
            .and_then(|result| result)
        };

        let finished =
            history_transition_status_from_sync_result(queued_at_ms, result, wait_warnings);
        set_history_transition_status_arc(&transition_status, finished.clone());
        let _ = app.emit(CODEX_HISTORY_SYNC_FINISHED_EVENT, finished);
    });

    queued
}

fn append_history_transition_status(
    message: String,
    status: &CodexHistoryTransitionStatus,
) -> String {
    format!("{message} {}", status.message)
}

fn set_history_transition_status(state: &AppState, status: CodexHistoryTransitionStatus) {
    set_history_transition_status_arc(&state.history_transition_status, status);
}

fn set_history_transition_status_arc(
    transition_status: &Arc<Mutex<Option<CodexHistoryTransitionStatus>>>,
    status: CodexHistoryTransitionStatus,
) {
    if let Ok(mut current) = transition_status.lock() {
        let should_update = match current.as_ref() {
            Some(existing) => status.queued_at_ms >= existing.queued_at_ms,
            None => true,
        };
        if should_update {
            *current = Some(status);
        }
    }
}

fn latest_history_transition_status(state: &AppState) -> Option<CodexHistoryTransitionStatus> {
    state
        .history_transition_status
        .lock()
        .ok()
        .and_then(|current| current.clone())
}

async fn wait_for_target_auth_json(
    target_home: &std::path::Path,
    before: Option<AuthFileFingerprint>,
    queued_at_ms: i64,
    warnings: &mut Vec<String>,
) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        if auth_json_ready_for_transition(target_home, before.as_ref(), queued_at_ms) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    warnings.push(format!(
        "{} auth.json 未在后台同步前确认更新，已继续同步。",
        target_home.display()
    ));
}

fn auth_json_ready_for_transition(
    target_home: &std::path::Path,
    before: Option<&AuthFileFingerprint>,
    queued_at_ms: i64,
) -> bool {
    let Some(current) = auth_json_fingerprint(target_home) else {
        return false;
    };
    let Some(before) = before else {
        return true;
    };
    if current.len != before.len {
        return true;
    }
    match (current.modified_at_ms, before.modified_at_ms) {
        (Some(current_modified), Some(before_modified)) if current_modified > before_modified => {
            true
        }
        (Some(current_modified), None) => current_modified >= queued_at_ms.saturating_sub(1_000),
        _ => false,
    }
}

fn auth_json_fingerprint(home: &std::path::Path) -> Option<AuthFileFingerprint> {
    let metadata = std::fs::metadata(home.join("auth.json")).ok()?;
    Some(AuthFileFingerprint {
        modified_at_ms: metadata
            .modified()
            .ok()
            .and_then(|modified| system_time_to_ms(modified).ok()),
        len: metadata.len(),
    })
}

fn system_time_to_ms(value: SystemTime) -> AppResult<i64> {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .map_err(|_| AppError::Internal)
}

fn history_transition_status_from_sync_result(
    queued_at_ms: i64,
    result: AppResult<CodexHistorySyncReport>,
    mut warnings: Vec<String>,
) -> CodexHistoryTransitionStatus {
    let completed_at_ms = Some(now_ms());
    match result {
        Ok(report) => {
            warnings.extend(report.warnings);
            let status = if warnings.is_empty() {
                "completed"
            } else {
                "warning"
            };
            let message = if warnings.is_empty() {
                "Codex 会话历史已在后台恢复。".to_owned()
            } else {
                format!(
                    "Codex 会话历史已在后台恢复，{} 项需要在会话页复查。",
                    warnings.len()
                )
            };
            CodexHistoryTransitionStatus {
                status: status.to_owned(),
                message,
                queued_at_ms,
                completed_at_ms,
                warnings,
            }
        }
        Err(error) => {
            warnings.push(error.to_string());
            CodexHistoryTransitionStatus {
                status: "warning".to_owned(),
                message: "Codex 会话历史后台恢复未完成，可在会话页手动同步/修复。".to_owned(),
                queued_at_ms,
                completed_at_ms,
                warnings,
            }
        }
    }
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
    Ok(DesktopWorkspaceSettings {
        mode: state.repository.desktop_workspace_mode()?,
    })
}

#[tauri::command]
pub fn app_update_settings(state: State<'_, AppState>) -> AppResult<AppUpdateSettings> {
    state.repository.app_update_settings()
}

#[tauri::command]
pub fn update_app_update_settings(
    input: UpdateAppUpdateSettingsInput,
    state: State<'_, AppState>,
) -> AppResult<AppUpdateSettings> {
    state
        .repository
        .set_app_update_settings(input.channel, input.auto_check)
}

#[tauri::command]
pub async fn check_app_update(
    input: CheckAppUpdateInput,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Option<AppUpdateInfo>> {
    let channel = input
        .channel
        .unwrap_or(state.repository.app_update_settings()?.channel);
    check_update_for_channel(&app, channel)
        .await
        .map(|update| update.map(|update| app_update_info(channel, &update)))
}

#[tauri::command]
pub async fn install_app_update(
    input: InstallAppUpdateInput,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let channel = input
        .channel
        .unwrap_or(state.repository.app_update_settings()?.channel);
    let Some(update) = check_update_for_channel(&app, channel).await? else {
        return Err(AppError::NotFound);
    };
    let update_info = app_update_info(channel, &update);
    emit_app_update_progress(
        &app,
        app_update_progress_event(
            AppUpdateProgressPhase::Checking,
            &update_info,
            AppUpdateDownloadProgress::default(),
            false,
            "正在准备下载更新。",
        ),
    );

    let progress = Arc::new(Mutex::new(AppUpdateDownloadProgress::default()));
    let app_for_chunk = app.clone();
    let update_for_chunk = update_info.clone();
    let progress_for_chunk = Arc::clone(&progress);
    let app_for_finish = app.clone();
    let update_for_finish = update_info.clone();
    let progress_for_finish = Arc::clone(&progress);

    let result = update
        .download_and_install(
            move |chunk_length, content_length| {
                let snapshot =
                    record_app_update_chunk(&progress_for_chunk, chunk_length, content_length);
                emit_app_update_progress(
                    &app_for_chunk,
                    app_update_progress_event(
                        AppUpdateProgressPhase::Downloading,
                        &update_for_chunk,
                        snapshot,
                        false,
                        "正在下载更新。",
                    ),
                );
            },
            move || {
                let snapshot = app_update_progress_snapshot(&progress_for_finish);
                emit_app_update_progress(
                    &app_for_finish,
                    app_update_progress_event(
                        AppUpdateProgressPhase::Downloaded,
                        &update_for_finish,
                        snapshot,
                        true,
                        "更新包已下载，正在校验并准备安装。",
                    ),
                );
                emit_app_update_progress(
                    &app_for_finish,
                    app_update_progress_event(
                        AppUpdateProgressPhase::Installing,
                        &update_for_finish,
                        snapshot,
                        true,
                        "正在安装更新。",
                    ),
                );
            },
        )
        .await;

    if result.is_err() {
        emit_app_update_progress(
            &app,
            app_update_progress_event(
                AppUpdateProgressPhase::Failed,
                &update_info,
                app_update_progress_snapshot(&progress),
                false,
                "更新下载或安装失败，请稍后重试。",
            ),
        );
        return Err(AppError::AppUpdateUnavailable);
    }

    emit_app_update_progress(
        &app,
        app_update_progress_event(
            AppUpdateProgressPhase::Restarting,
            &update_info,
            app_update_progress_snapshot(&progress),
            true,
            "更新已安装，应用即将重启。",
        ),
    );
    app.restart();
}

async fn check_update_for_channel(
    app: &AppHandle,
    channel: AppUpdateChannel,
) -> AppResult<Option<Update>> {
    let endpoint = url::Url::parse(channel.endpoint()).map_err(|_| AppError::Internal)?;
    app.updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|_| AppError::AppUpdateUnavailable)?
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| AppError::AppUpdateUnavailable)?
        .check()
        .await
        .map_err(|_| AppError::AppUpdateUnavailable)
}

fn app_update_info(channel: AppUpdateChannel, update: &Update) -> AppUpdateInfo {
    AppUpdateInfo {
        version: update.version.clone(),
        current_version: update.current_version.clone(),
        body: update.body.clone(),
        date: update.date.as_ref().map(ToString::to_string),
        channel,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AppUpdateDownloadProgress {
    downloaded_bytes: u64,
    content_length: Option<u64>,
}

fn record_app_update_chunk(
    progress: &Mutex<AppUpdateDownloadProgress>,
    chunk_length: usize,
    content_length: Option<u64>,
) -> AppUpdateDownloadProgress {
    let mut progress = progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    progress.downloaded_bytes = progress
        .downloaded_bytes
        .saturating_add(chunk_length as u64);
    if content_length.is_some() {
        progress.content_length = content_length;
    }
    *progress
}

fn app_update_progress_snapshot(
    progress: &Mutex<AppUpdateDownloadProgress>,
) -> AppUpdateDownloadProgress {
    *progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn app_update_progress_event(
    phase: AppUpdateProgressPhase,
    update: &AppUpdateInfo,
    progress: AppUpdateDownloadProgress,
    complete: bool,
    message: impl Into<String>,
) -> AppUpdateProgressEvent {
    AppUpdateProgressEvent {
        phase,
        channel: update.channel,
        version: update.version.clone(),
        current_version: update.current_version.clone(),
        downloaded_bytes: progress.downloaded_bytes,
        content_length: progress.content_length,
        progress_percent: app_update_progress_percent(
            progress.downloaded_bytes,
            progress.content_length,
            complete,
        ),
        message: message.into(),
        updated_at_ms: now_ms(),
    }
}

fn emit_app_update_progress(app: &AppHandle, event: AppUpdateProgressEvent) {
    let _ = app.emit(APP_UPDATE_PROGRESS_EVENT, event);
}

fn app_update_progress_percent(
    downloaded_bytes: u64,
    content_length: Option<u64>,
    complete: bool,
) -> Option<u8> {
    if complete {
        return Some(100);
    }
    let total = content_length.filter(|total| *total > 0)?;
    Some(((downloaded_bytes.min(total) * 100) / total).min(100) as u8)
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
    let credential = active_profile_credential(state.inner(), &profile).await?;
    state
        .repository
        .touch_desktop_workspace(&workspace.id, now_ms())?;
    match credential {
        ActiveProfileCredential::OAuth(credential) => state.runtime.activate_profile(
            profile,
            credential,
            DesktopWorkspaceLaunch::Fresh(workspace.id),
        ),
        ActiveProfileCredential::AuthJson(auth_json) => state.runtime.activate_profile_auth_json(
            profile,
            auth_json,
            DesktopWorkspaceLaunch::Fresh(workspace.id),
        ),
    }
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
    let mut activation = state
        .runtime
        .current_profile_activation_status(&input.attempt_id)?;
    persist_current_profile_if_activated(&state.repository, &activation)?;
    if let Some(history_sync_status) = latest_history_transition_status(state.inner()) {
        activation.message =
            append_history_transition_status(activation.message, &history_sync_status);
        activation.history_sync_status = Some(history_sync_status);
    }
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
    match active_profile_credential(state.inner(), &profile).await? {
        ActiveProfileCredential::OAuth(credential) => state
            .runtime
            .start_managed_task(&profile, credential, input),
        ActiveProfileCredential::AuthJson(auth_json) => state
            .runtime
            .start_managed_task_auth_json(&profile, auth_json, input),
    }
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
pub async fn update_gateway(
    input: UpdateGatewayInput,
    state: State<'_, AppState>,
) -> AppResult<GatewayStatus> {
    let (bind_mode, bind_address, cidrs) = match input.bind_mode.as_str() {
        "loopback" => ("loopback".to_owned(), "127.0.0.1".to_owned(), Vec::new()),
        "lan" if input.confirmed_lan => {
            let address = input
                .bind_address
                .parse()
                .map_err(|_| AppError::ValidationFailed)?;
            validate_binding(&input.bind_mode, address, &input.cidrs)?;
            if !available_lan_addresses()
                .iter()
                .any(|candidate| candidate.address == input.bind_address)
            {
                return Err(AppError::ForbiddenNetworkTarget);
            }
            (
                input.bind_mode.clone(),
                input.bind_address.clone(),
                input.cidrs.clone(),
            )
        }
        "lan" => return Err(AppError::ConfirmationRequired),
        _ => return Err(AppError::ValidationFailed),
    };
    let address = bind_address
        .parse()
        .map_err(|_| AppError::ValidationFailed)?;
    validate_binding(&bind_mode, address, &cidrs)?;
    if state.gateway.is_running() {
        return Err(AppError::Conflict);
    }
    configure_gateway_upstream_proxy(
        &state.repository,
        state.secrets.clone(),
        input.upstream_proxy_mode.as_deref(),
        input.upstream_proxy_url.as_deref(),
    )
    .await?;
    state
        .repository
        .update_gateway_settings(&bind_mode, &bind_address, input.port, &cidrs)?;
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
pub fn export_gateway_ca(destination: String, state: State<'_, AppState>) -> AppResult<()> {
    state.gateway.export_ca(std::path::Path::new(&destination))
}

#[tauri::command]
pub fn trust_gateway_ca(state: State<'_, AppState>) -> AppResult<()> {
    state.gateway.trust_ca_in_system_store()
}

#[tauri::command]
pub async fn codex_gateway_config_status(
    state: State<'_, AppState>,
) -> AppResult<GatewayCodexConfigStatus> {
    codex_gateway::status(&state.repository).await
}

#[tauri::command]
pub async fn enable_codex_gateway(
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<GatewayCodexConfigStatus> {
    let oauth_profile_id = state
        .repository
        .setting(crate::domain::GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)?;
    validate_oauth_profile_for_switch(state.inner(), oauth_profile_id.as_deref()).await?;
    let gateway_status = gateway_ready_for_codex_switch(state.inner()).await?;
    let mut status = codex_gateway::enable(
        &state.repository,
        state.secrets.clone(),
        state.oauth_credentials.clone(),
        gateway_status,
        &state.data_dir,
    )
    .await?;
    let history_sync_status = queue_codex_history_transition_sync(state.inner(), &app, None, None);
    status.message = append_history_transition_status(status.message, &history_sync_status);
    status.history_sync_status = Some(history_sync_status);
    append_shared_desktop_restart_result(state.inner(), &mut status);
    Ok(status)
}

#[tauri::command]
pub async fn disable_codex_gateway(
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<GatewayCodexConfigStatus> {
    let mut status = codex_gateway::disable(&state.repository, state.secrets.clone()).await?;
    let history_sync_status = queue_codex_history_transition_sync(state.inner(), &app, None, None);
    status.message = append_history_transition_status(status.message, &history_sync_status);
    status.history_sync_status = Some(history_sync_status);
    append_shared_desktop_restart_result(state.inner(), &mut status);
    Ok(status)
}

#[tauri::command]
pub async fn set_codex_gateway_oauth_profile(
    input: SetCodexGatewayOAuthProfileInput,
    state: State<'_, AppState>,
) -> AppResult<GatewayCodexConfigStatus> {
    validate_oauth_profile_for_switch(state.inner(), input.profile_id.as_deref()).await?;
    let mut status =
        codex_gateway::set_codex_oauth_profile(&state.repository, &state.oauth_credentials, input)
            .await?;
    if status.enabled {
        append_shared_desktop_restart_result(state.inner(), &mut status);
    }
    Ok(status)
}

#[tauri::command]
pub fn list_gateway_model_options(state: State<'_, AppState>) -> AppResult<Vec<String>> {
    codex_gateway::gateway_model_options(&state.repository)
}

#[tauri::command]
pub async fn activate_api_service_profile(
    id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<GatewayCodexConfigStatus> {
    prepare_api_profile_for_codex_switch(&state.repository, state.secrets.clone(), &id).await?;
    let direct_profile = state.repository.profile(&id)?;
    validate_oauth_profile_for_switch(
        state.inner(),
        direct_profile.profile.codex_oauth_profile_id.as_deref(),
    )
    .await?;
    let mut status = codex_gateway::enable_api_profile(
        &state.repository,
        state.secrets.clone(),
        state.oauth_credentials.clone(),
        &id,
        &state.data_dir,
    )
    .await?;
    let history_sync_status = queue_codex_history_transition_sync(state.inner(), &app, None, None);
    status.message = append_history_transition_status(status.message, &history_sync_status);
    status.history_sync_status = Some(history_sync_status);
    append_shared_desktop_restart_result(state.inner(), &mut status);
    Ok(status)
}

async fn validate_oauth_profile_for_switch(
    state: &AppState,
    profile_id: Option<&str>,
) -> AppResult<()> {
    let Some(profile_id) = profile_id else {
        return Ok(());
    };
    let profile =
        sync_profile_quota_or_mark_stale(state, profile_id, OAuthCredentialAccess::UserInitiated)
            .await?;
    if profile.validation_status == "invalid" {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    Ok(())
}

async fn prepare_api_profile_for_codex_switch(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
) -> AppResult<()> {
    let stored = repository.profile(id)?;
    if stored.profile.kind != crate::domain::ProfileKind::ApiKey
        || !matches!(
            stored.profile.provider,
            GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible
        )
        || stored.profile.wire_api != GatewayWireApi::Responses
    {
        return Err(AppError::ValidationFailed);
    }
    discover_profile_models(repository, secrets, id).await?;
    Ok(())
}

async fn gateway_ready_for_codex_switch(state: &AppState) -> AppResult<GatewayStatus> {
    let mut gateway_status = state.gateway.status()?;
    if !gateway_status.running || !gateway_status.certificate_ready {
        gateway_status = state.gateway.start().await?;
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        state.gateway.trust_ca_in_system_store()?;
    }
    Ok(gateway_status)
}

fn append_shared_desktop_restart_result(state: &AppState, status: &mut GatewayCodexConfigStatus) {
    let suffix = match state.runtime.restart_default_desktop_for_shared_switch() {
        Ok(()) => {
            "已复用原客户端数据目录重启 ChatGPT/Codex，聊天记录、记忆、设置与状态会保留。"
        }
        Err(_) => {
            "配置已写入；请手动重启 ChatGPT/Codex，Relay 未传入独立客户端数据目录，聊天记录、记忆、设置与状态会保留。"
        }
    };
    status.message = format!("{} {}", status.message, suffix);
}

#[tauri::command]
pub async fn refresh_profile_models(
    id: String,
    state: State<'_, AppState>,
) -> AppResult<MaskedProfile> {
    let mut profile = state.repository.profile(&id)?;
    if profile.profile.kind == crate::domain::ProfileKind::ApiKey {
        return discover_profile_models(&state.repository, state.secrets.clone(), &id).await;
    }
    if profile.profile.auth_mode != CodexAuthMode::OAuth {
        return Err(AppError::ValidationFailed);
    }
    let credential = state
        .oauth_credentials
        .current_or_refresh(&profile, OAuthCredentialAccess::UserInitiated)
        .await?;
    let result = state.runtime.read_profile_models(&id, credential).await?;
    state
        .oauth_credentials
        .persist(
            &profile,
            &result.credential,
            OAuthCredentialAccess::UserInitiated,
        )
        .await?;
    profiles::save_oauth_credential_metadata(&state.repository, &id, &result.credential)?;
    profile = state.repository.profile(&id)?;
    profile.profile.models = result.models;
    profile.profile.health = "healthy".to_owned();
    state.repository.update_profile(&profile)?;
    Ok(profile.profile)
}

#[tauri::command]
pub async fn test_api_service_profile(
    input: TestApiServiceInput,
) -> AppResult<crate::domain::ApiServiceTestReport> {
    test_api_service(&input.provider, &input.base_url, &input.api_key).await
}

#[tauri::command]
pub async fn test_existing_api_service_profile(
    id: String,
    state: State<'_, AppState>,
) -> AppResult<crate::domain::ApiServiceTestReport> {
    let profile = state.repository.profile(&id)?;
    if profile.profile.kind != crate::domain::ProfileKind::ApiKey {
        return Err(AppError::ValidationFailed);
    }
    let base_url = profile
        .profile
        .base_url
        .as_deref()
        .ok_or(AppError::ValidationFailed)?;
    let secret_ref = profile
        .secret_ref
        .as_deref()
        .ok_or(AppError::ValidationFailed)?;
    let key = state.secrets.get(secret_ref).await?;
    test_api_service(&profile.profile.provider, base_url, &key).await
}

#[tauri::command]
pub fn codex_environment_status(state: State<'_, AppState>) -> AppResult<CodexEnvironmentReport> {
    codex_environment::status(Some(state.data_dir.join("certs/gateway-ca.pem")))
}

#[tauri::command]
pub fn install_codex_environment(
    input: InstallCodexEnvironmentInput,
    state: State<'_, AppState>,
) -> AppResult<CodexEnvironmentInstallReport> {
    codex_environment::install(input, Some(state.data_dir.join("certs/gateway-ca.pem")))
}

#[tauri::command]
pub fn list_client_keys(state: State<'_, AppState>) -> AppResult<Vec<MaskedClientKey>> {
    state.repository.list_client_keys()
}

fn generated_client_key_material() -> AppResult<(String, String, String)> {
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
    Ok((plaintext_once, hash, masked_value))
}

#[tauri::command]
pub async fn reveal_client_key(
    input: ClientKeySecretInput,
    state: State<'_, AppState>,
) -> AppResult<String> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let secret_ref = state.repository.user_client_key_secret_ref(&input.id)?;
    state.secrets.get(&secret_ref).await
}

#[tauri::command]
pub async fn rotate_client_key(
    input: ClientKeySecretInput,
    state: State<'_, AppState>,
) -> AppResult<CreatedClientKey> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let (plaintext_once, hash, masked_value) = generated_client_key_material()?;
    let secret_ref = state.repository.user_client_key_secret_ref(&input.id)?;
    let previous_secret = state.secrets.get(&secret_ref).await.ok();
    state.secrets.set(&secret_ref, &plaintext_once).await?;
    if let Err(error) = state
        .repository
        .update_client_key_material(&input.id, &hash, &masked_value)
    {
        if let Some(previous_secret) = previous_secret {
            let _ = state.secrets.set(&secret_ref, &previous_secret).await;
        }
        return Err(error);
    }
    let key = state
        .repository
        .list_client_keys()?
        .into_iter()
        .find(|key| key.id == input.id)
        .ok_or(AppError::NotFound)?;
    Ok(CreatedClientKey {
        key,
        plaintext_once,
    })
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
    let (plaintext_once, hash, masked_value) = generated_client_key_material()?;
    let secret_ref = format!("client-key:{id}");
    state.secrets.set(&secret_ref, &plaintext_once).await?;
    let key = MaskedClientKey {
        id,
        name: input.name.trim().to_owned(),
        masked_value,
        created_at_ms: now_ms(),
        last_used_at_ms: None,
        revoked: false,
        managed_by: "user".to_owned(),
        can_revoke: true,
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
pub async fn list_collaboration_bots(
    state: State<'_, AppState>,
) -> AppResult<Vec<MaskedCollaborationBot>> {
    state.collaboration.list_bots().await
}

#[tauri::command]
pub async fn upsert_collaboration_bot(
    input: UpsertCollaborationBotInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedCollaborationBot> {
    state.collaboration.upsert_bot(input).await
}

#[tauri::command]
pub async fn test_collaboration_bot(
    id: String,
    state: State<'_, AppState>,
) -> AppResult<MaskedCollaborationBot> {
    state.collaboration.test_bot(id).await
}

#[tauri::command]
pub async fn delete_collaboration_bot(
    input: DeleteCollaborationBotInput,
    state: State<'_, AppState>,
) -> AppResult<()> {
    state.collaboration.delete_bot(input).await
}

#[tauri::command]
pub fn list_collaboration_project_bindings(
    state: State<'_, AppState>,
) -> AppResult<Vec<CollaborationProjectBinding>> {
    state.collaboration.list_bindings()
}

#[tauri::command]
pub fn upsert_collaboration_project_binding(
    input: UpsertCollaborationProjectBindingInput,
    state: State<'_, AppState>,
) -> AppResult<CollaborationProjectBinding> {
    state.collaboration.upsert_binding(input)
}

#[tauri::command]
pub fn delete_collaboration_project_binding(
    input: DeleteCollaborationProjectBindingInput,
    state: State<'_, AppState>,
) -> AppResult<()> {
    state.collaboration.delete_binding(input)
}

#[tauri::command]
pub async fn register_discord_commands(
    id: String,
    state: State<'_, AppState>,
) -> AppResult<MaskedCollaborationBot> {
    state.collaboration.register_discord_commands(id).await
}

#[tauri::command]
pub fn collaboration_callback_status(
    state: State<'_, AppState>,
) -> AppResult<CollaborationCallbackStatus> {
    state.collaboration.callback_status()
}

#[tauri::command]
pub async fn list_feishu_bots(state: State<'_, AppState>) -> AppResult<Vec<MaskedFeishuBot>> {
    Ok(state
        .collaboration
        .list_bots()
        .await?
        .into_iter()
        .filter(|bot| bot.provider == CollaborationProvider::Feishu)
        .map(feishu_bot_from_collaboration)
        .collect())
}

#[tauri::command]
pub async fn upsert_feishu_bot(
    input: UpsertFeishuBotInput,
    state: State<'_, AppState>,
) -> AppResult<MaskedFeishuBot> {
    let bot = state
        .collaboration
        .upsert_bot(UpsertCollaborationBotInput {
            id: input.id,
            provider: CollaborationProvider::Feishu,
            name: input.name,
            enabled: input.enabled,
            confirmed: input.confirmed,
            app_id: Some(input.app_id),
            app_secret: input.app_secret,
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
        })
        .await?;
    Ok(feishu_bot_from_collaboration(bot))
}

#[tauri::command]
pub async fn test_feishu_bot(id: String, state: State<'_, AppState>) -> AppResult<MaskedFeishuBot> {
    state
        .collaboration
        .test_bot(id)
        .await
        .map(feishu_bot_from_collaboration)
}

#[tauri::command]
pub async fn delete_feishu_bot(
    input: DeleteFeishuBotInput,
    state: State<'_, AppState>,
) -> AppResult<()> {
    state
        .collaboration
        .delete_bot(DeleteCollaborationBotInput {
            id: input.id,
            confirmed: input.confirmed,
        })
        .await
}

#[tauri::command]
pub fn list_feishu_project_bindings(
    state: State<'_, AppState>,
) -> AppResult<Vec<FeishuProjectBinding>> {
    Ok(state
        .collaboration
        .list_bindings()?
        .into_iter()
        .filter(|binding| binding.provider == CollaborationProvider::Feishu)
        .map(feishu_binding_from_collaboration)
        .collect())
}

#[tauri::command]
pub fn upsert_feishu_project_binding(
    input: UpsertFeishuProjectBindingInput,
    state: State<'_, AppState>,
) -> AppResult<FeishuProjectBinding> {
    state
        .collaboration
        .upsert_binding(UpsertCollaborationProjectBindingInput {
            id: input.id,
            bot_id: input.bot_id,
            project_name: input.project_name,
            project_slug: input.project_slug,
            working_directory: input.working_directory,
            profile_id: Some(input.profile_id),
            enabled: input.enabled,
            concurrency_limit: input.concurrency_limit,
            execution_target: Some("profile".to_owned()),
            model_id: None,
            confirmed: input.confirmed,
        })
        .map(feishu_binding_from_collaboration)
}

#[tauri::command]
pub fn delete_feishu_project_binding(
    input: DeleteFeishuProjectBindingInput,
    state: State<'_, AppState>,
) -> AppResult<()> {
    state
        .collaboration
        .delete_binding(DeleteCollaborationProjectBindingInput {
            id: input.id,
            confirmed: input.confirmed,
        })
}

#[tauri::command]
pub fn list_collaboration_contexts(
    state: State<'_, AppState>,
) -> AppResult<Vec<CollaborationContextSummary>> {
    state.collaboration.list_contexts()
}

#[tauri::command]
pub fn update_collaboration_context(
    input: UpdateCollaborationContextInput,
    state: State<'_, AppState>,
) -> AppResult<CollaborationContextSummary> {
    state.collaboration.update_context(input)
}

#[tauri::command]
pub fn reset_collaboration_context(
    input: ResetCollaborationContextInput,
    state: State<'_, AppState>,
) -> AppResult<CollaborationContextSummary> {
    state.collaboration.reset_context(input)
}

#[tauri::command]
pub fn list_codex_sessions(
    input: ListCodexSessionsInput,
    state: State<'_, AppState>,
) -> AppResult<Vec<CodexSessionSummary>> {
    state.collaboration.list_sessions(input)
}

#[tauri::command]
pub async fn cancel_codex_session(
    input: CancelCodexSessionInput,
    state: State<'_, AppState>,
) -> AppResult<CodexSessionSummary> {
    state.collaboration.cancel_session(input).await
}

#[tauri::command]
pub async fn continue_codex_session(
    input: ContinueCodexSessionInput,
    state: State<'_, AppState>,
) -> AppResult<CodexSessionSummary> {
    state.collaboration.continue_session(input).await
}

fn feishu_bot_from_collaboration(bot: MaskedCollaborationBot) -> MaskedFeishuBot {
    MaskedFeishuBot {
        id: bot.id,
        name: bot.name,
        app_id: bot.config_summary.clone(),
        app_id_mask: bot.credential_mask,
        enabled: bot.enabled,
        connection_status: bot.connection_status,
        last_error: bot.last_error,
        updated_at_ms: bot.updated_at_ms,
    }
}

fn feishu_binding_from_collaboration(binding: CollaborationProjectBinding) -> FeishuProjectBinding {
    FeishuProjectBinding {
        id: binding.id,
        bot_id: binding.bot_id,
        bot_name: binding.bot_name,
        project_name: binding.project_name,
        project_slug: binding.project_slug,
        working_directory: binding.working_directory,
        profile_id: binding.profile_id.unwrap_or_default(),
        profile_alias: binding.profile_alias.unwrap_or_default(),
        chat_id: binding.chat_id,
        bind_code: binding.bind_code,
        enabled: binding.enabled,
        concurrency_limit: binding.concurrency_limit,
        created_at_ms: binding.created_at_ms,
        updated_at_ms: binding.updated_at_ms,
    }
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
    state.oauth_credentials.load(profile, access).await
}

async fn imported_auth_json(
    state: &AppState,
    profile: &crate::database::StoredProfile,
    access: OAuthCredentialAccess,
) -> AppResult<String> {
    if profile.profile.kind != crate::domain::ProfileKind::CodexOauth
        || profile.profile.auth_mode == CodexAuthMode::OAuth
    {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    let reference = profile
        .secret_ref
        .as_deref()
        .ok_or(AppError::ProfileRuntimeUnavailable)?;
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
    let credential: crate::profiles::ImportedAuthFileCredential =
        serde_json::from_str(&value).map_err(|_| AppError::ProfileRuntimeUnavailable)?;
    if credential.auth_mode != profile.profile.auth_mode || credential.auth_json.trim().is_empty() {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    Ok(credential.auth_json)
}

enum ActiveProfileCredential {
    OAuth(crate::profiles::CodexOAuthCredential),
    AuthJson(String),
}

async fn active_profile_credential(
    state: &AppState,
    profile: &crate::database::StoredProfile,
) -> AppResult<ActiveProfileCredential> {
    if profile.profile.auth_mode == CodexAuthMode::OAuth {
        if let Ok(credential) =
            oauth_credential(state, profile, OAuthCredentialAccess::UserInitiated).await
        {
            return Ok(ActiveProfileCredential::OAuth(credential));
        }
    }
    let reference = profile
        .secret_ref
        .as_deref()
        .ok_or(AppError::ProfileRuntimeUnavailable)?;
    let value = state
        .secrets
        .get(reference)
        .await
        .map_err(|error| match error {
            AppError::NotFound => AppError::ProfileRuntimeUnavailable,
            other => other,
        })?;
    let credential: crate::profiles::ImportedAuthFileCredential =
        serde_json::from_str(&value).map_err(|_| AppError::ProfileRuntimeUnavailable)?;
    if credential.auth_mode != profile.profile.auth_mode || credential.auth_json.is_empty() {
        return Err(AppError::ProfileRuntimeUnavailable);
    }
    Ok(ActiveProfileCredential::AuthJson(credential.auth_json))
}

async fn persist_oauth_credential(
    state: &AppState,
    profile: &crate::database::StoredProfile,
    credential: &crate::profiles::CodexOAuthCredential,
    access: OAuthCredentialAccess,
) -> AppResult<()> {
    state
        .oauth_credentials
        .persist(profile, credential, access)
        .await
}

fn cache_oauth_credential(
    state: &AppState,
    profile_id: &str,
    credential: crate::profiles::CodexOAuthCredential,
    persistence_pending: bool,
) -> AppResult<()> {
    state
        .oauth_credentials
        .insert(profile_id, credential, persistence_pending)
}

fn remove_cached_oauth_credential(state: &AppState, profile_id: &str) -> AppResult<()> {
    state.oauth_credentials.remove(profile_id)
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
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        thread,
    };

    #[test]
    fn app_update_progress_percent_handles_known_unknown_and_complete_downloads() {
        assert_eq!(
            app_update_progress_percent(512, Some(1024), false),
            Some(50)
        );
        assert_eq!(
            app_update_progress_percent(2048, Some(1024), false),
            Some(100)
        );
        assert_eq!(app_update_progress_percent(512, None, false), None);
        assert_eq!(app_update_progress_percent(0, Some(0), false), None);
        assert_eq!(app_update_progress_percent(0, None, true), Some(100));
    }

    #[test]
    fn app_update_progress_event_marks_complete_and_failed_phases() {
        let update = AppUpdateInfo {
            version: "0.2.0-beta.6".to_owned(),
            current_version: "0.2.0-beta.5".to_owned(),
            body: None,
            date: None,
            channel: AppUpdateChannel::Beta,
        };

        let downloaded = app_update_progress_event(
            AppUpdateProgressPhase::Downloaded,
            &update,
            AppUpdateDownloadProgress {
                downloaded_bytes: 900,
                content_length: None,
            },
            true,
            "下载完成",
        );
        assert_eq!(downloaded.progress_percent, Some(100));
        assert_eq!(downloaded.phase, AppUpdateProgressPhase::Downloaded);
        assert_eq!(downloaded.channel, AppUpdateChannel::Beta);

        let failed = app_update_progress_event(
            AppUpdateProgressPhase::Failed,
            &update,
            AppUpdateDownloadProgress {
                downloaded_bytes: 250,
                content_length: Some(1000),
            },
            false,
            "更新失败",
        );
        assert_eq!(failed.progress_percent, Some(25));
        assert_eq!(failed.phase, AppUpdateProgressPhase::Failed);
        assert_eq!(failed.message, "更新失败");
    }

    #[test]
    fn transition_auth_readiness_waits_for_existing_auth_json_update() {
        let root = temp_command_root("auth-ready");
        let home = root.join("runtime-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "old").unwrap();
        let before = auth_json_fingerprint(&home);

        assert!(!auth_json_ready_for_transition(
            &home,
            before.as_ref(),
            now_ms().saturating_add(5_000),
        ));

        std::fs::write(home.join("auth.json"), "new-auth-json").unwrap();

        assert!(auth_json_ready_for_transition(
            &home,
            before.as_ref(),
            now_ms(),
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transition_sync_failure_becomes_warning_status() {
        let status = history_transition_status_from_sync_result(
            42,
            Err(AppError::RuntimeUnavailable),
            Vec::new(),
        );

        assert_eq!(status.status, "warning");
        assert!(status.completed_at_ms.is_some());
        assert_eq!(status.queued_at_ms, 42);
        assert_eq!(status.warnings.len(), 1);
        assert!(status.message.contains("手动同步/修复"));
    }

    #[test]
    fn older_transition_status_does_not_replace_newer_status() {
        let store = Arc::new(Mutex::new(None));
        set_history_transition_status_arc(
            &store,
            CodexHistoryTransitionStatus {
                status: "running".to_owned(),
                message: "new".to_owned(),
                queued_at_ms: 10,
                completed_at_ms: None,
                warnings: Vec::new(),
            },
        );
        set_history_transition_status_arc(
            &store,
            CodexHistoryTransitionStatus {
                status: "completed".to_owned(),
                message: "old".to_owned(),
                queued_at_ms: 9,
                completed_at_ms: Some(11),
                warnings: Vec::new(),
            },
        );

        let current = store.lock().unwrap().clone().unwrap();
        assert_eq!(current.message, "new");
    }

    fn activation(status: &str) -> CurrentProfileActivation {
        CurrentProfileActivation {
            profile_id: "oauth-profile".to_owned(),
            attempt_id: Some("attempt".to_owned()),
            status: status.to_owned(),
            message: "非敏感状态".to_owned(),
            history_sync: None,
            history_sync_status: None,
        }
    }

    fn temp_command_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("codex-command-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn current_profile_selection_requires_confirmation_and_uses_shared_workspace() {
        assert!(matches!(
            desktop_workspace_for_current_profile_selection(&SelectCurrentProfileInput {
                id: "oauth-profile".to_owned(),
                confirmed_desktop_restart: false,
            }),
            Err(AppError::ConfirmationRequired)
        ));
        assert!(matches!(
            desktop_workspace_for_current_profile_selection(&SelectCurrentProfileInput {
                id: "oauth-profile".to_owned(),
                confirmed_desktop_restart: true,
            })
            .unwrap(),
            DesktopWorkspaceLaunch::Shared
        ));
    }

    #[tokio::test]
    async fn prepares_api_profile_for_codex_switch_by_refreshing_models() {
        let repository = Repository::memory();
        let secrets = Arc::new(crate::secrets::MemorySecretStore::new());
        let profile = profiles::create_profile(
            &repository,
            secrets.clone(),
            CreateProfileInput {
                alias: "Third party".to_owned(),
                kind: crate::domain::ProfileKind::ApiKey,
                base_url: Some(spawn_openai_models_server(&["third-party-coder"])),
                provider: crate::domain::GatewayProvider::OpenAiCompatible,
                wire_api: crate::domain::GatewayWireApi::Responses,
                api_key: Some("sk-test".to_owned()),
                models: vec!["stale-model".to_owned()],
                model_mappings: Vec::new(),
                codex_oauth_profile_id: None,
                in_pool: false,
                priority: 0,
                weight: 1,
            },
        )
        .await
        .unwrap();
        let mut stored = repository.profile(&profile.id).unwrap();
        stored.profile.health = "unhealthy".to_owned();
        stored.profile.models.clear();
        stored.profile.model_mappings.clear();
        repository.update_profile(&stored).unwrap();

        prepare_api_profile_for_codex_switch(&repository, secrets, &profile.id)
            .await
            .unwrap();

        let refreshed = repository.profile(&profile.id).unwrap().profile;
        assert_eq!(refreshed.health, "healthy");
        assert_eq!(refreshed.models, vec!["third-party-coder"]);
        assert!(!refreshed.in_pool);
    }

    fn spawn_openai_models_server(models: &[&str]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = serde_json::json!({
            "data": models
                .iter()
                .map(|model| serde_json::json!({"id": model}))
                .collect::<Vec<_>>()
        })
        .to_string();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/v1")
    }

    fn test_app_state(repository: Arc<Repository>, secrets: Arc<dyn SecretStore>) -> AppState {
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        let gateway = Arc::new(GatewayManager::new(
            repository.clone(),
            secrets.clone(),
            oauth_credentials.clone(),
            PathBuf::from("/tmp/codex-relay-test-certs"),
        ));
        AppState {
            gateway: gateway.clone(),
            runtime: Arc::new(CodexRuntime::new(
                PathBuf::from("/tmp/codex-relay-test-runtime"),
                secrets.clone(),
            )),
            collaboration: Arc::new(CollaborationManager::new(
                repository.clone(),
                secrets.clone(),
                oauth_credentials.clone(),
                gateway,
                PathBuf::from("/tmp/codex-relay-test-data"),
            )),
            repository,
            data_dir: PathBuf::from("/tmp/codex-relay-test-data"),
            secrets: secrets.clone(),
            quota_refresh_lock: tokio::sync::Mutex::new(()),
            history_sync_lock: Arc::new(tokio::sync::Mutex::new(())),
            history_transition_status: Arc::new(Mutex::new(None)),
            oauth_credentials,
            json_imports: crate::profile_import::JsonProfileImportStore::default(),
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
        secrets.reset_counts();
        assert!(oauth_credential(
            &state,
            &repository.profile("oauth-profile").unwrap(),
            OAuthCredentialAccess::Background,
        )
        .await
        .is_ok());
        assert_eq!(secrets.silent_gets.load(Ordering::Relaxed), 0);
    }
}
