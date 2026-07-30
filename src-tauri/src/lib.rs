mod codex_environment;
mod codex_gateway;
mod codex_runtime;
mod collaboration;
mod commands;
mod database;
mod domain;
mod error;
mod gateway;
mod oauth_credentials;
mod profile_import;
mod profiles;
mod secrets;

use std::{error::Error, path::Path, sync::Arc};

#[cfg(debug_assertions)]
use std::time::Instant;

use collaboration::CollaborationManager;
use commands::AppState;
use database::Repository;
use gateway::GatewayManager;
use oauth_credentials::OAuthCredentialStore;
use secrets::{LocalEncryptedSecretStore, SecretStore};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_rustls_provider();

    let application = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| initialise(app))
        .invoke_handler(tauri::generate_handler![
            commands::dashboard_snapshot,
            commands::list_profiles,
            commands::create_profile,
            commands::create_api_service_profile,
            commands::update_profile,
            commands::sync_profile_account_info,
            commands::refresh_profile_quotas,
            commands::delete_profile,
            commands::select_current_profile,
            commands::desktop_workspace_settings,
            commands::update_desktop_workspace_settings,
            commands::app_update_settings,
            commands::update_app_update_settings,
            commands::check_app_update,
            commands::install_app_update,
            commands::list_desktop_workspaces,
            commands::restore_desktop_workspace,
            commands::delete_desktop_workspace,
            commands::current_profile_activation_status,
            commands::managed_task_status,
            commands::start_managed_task,
            commands::cancel_managed_task,
            commands::gateway_status,
            commands::update_gateway,
            commands::start_gateway,
            commands::stop_gateway,
            commands::export_gateway_ca,
            commands::trust_gateway_ca,
            commands::refresh_profile_models,
            commands::test_api_service_profile,
            commands::test_existing_api_service_profile,
            commands::codex_environment_status,
            commands::install_codex_environment,
            commands::codex_gateway_config_status,
            commands::enable_codex_gateway,
            commands::disable_codex_gateway,
            commands::set_codex_gateway_oauth_profile,
            commands::list_gateway_model_options,
            commands::activate_api_service_profile,
            commands::list_client_keys,
            commands::create_client_key,
            commands::reveal_client_key,
            commands::rotate_client_key,
            commands::revoke_client_key,
            commands::list_collaboration_bots,
            commands::upsert_collaboration_bot,
            commands::test_collaboration_bot,
            commands::delete_collaboration_bot,
            commands::list_collaboration_project_bindings,
            commands::upsert_collaboration_project_binding,
            commands::delete_collaboration_project_binding,
            commands::register_discord_commands,
            commands::collaboration_callback_status,
            commands::list_feishu_bots,
            commands::upsert_feishu_bot,
            commands::test_feishu_bot,
            commands::delete_feishu_bot,
            commands::list_feishu_project_bindings,
            commands::upsert_feishu_project_binding,
            commands::delete_feishu_project_binding,
            commands::list_collaboration_contexts,
            commands::update_collaboration_context,
            commands::reset_collaboration_context,
            commands::list_codex_sessions,
            commands::cancel_codex_session,
            commands::continue_codex_session,
            commands::start_oauth_import,
            commands::oauth_import_status,
            commands::cancel_oauth_import,
            commands::complete_oauth_import,
            commands::preview_json_profile_import,
            commands::commit_json_profile_import,
            commands::retry_json_profile_import,
            commands::discard_json_profile_import
        ])
        .build(tauri::generate_context!())
        .expect("error while building Tauri application");

    application.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Ready) {
            start_post_startup_tasks(app_handle);
        }
    });
}

fn install_rustls_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

fn initialise(app: &tauri::App) -> Result<(), Box<dyn Error>> {
    #[cfg(debug_assertions)]
    let started_at = Instant::now();
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let repository = Arc::new(
        Repository::open(&data_dir.join("relay.sqlite"))
            .map_err(|error| std::io::Error::other(error.to_string()))?,
    );
    let secrets: Arc<dyn SecretStore> = Arc::new(
        LocalEncryptedSecretStore::open(&data_dir)
            .map_err(|error| std::io::Error::other(error.to_string()))?,
    );
    let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
    let gateway = Arc::new(GatewayManager::new(
        repository.clone(),
        secrets.clone(),
        oauth_credentials.clone(),
        data_dir.join("certs"),
    ));
    let runtime = Arc::new(codex_runtime::CodexRuntime::new(
        data_dir.clone(),
        secrets.clone(),
    ));
    let collaboration = Arc::new(CollaborationManager::new(
        repository.clone(),
        secrets.clone(),
        oauth_credentials.clone(),
        gateway.clone(),
        data_dir.clone(),
    ));
    app.manage(AppState {
        repository,
        data_dir,
        secrets,
        gateway,
        runtime,
        collaboration,
        quota_refresh_lock: tokio::sync::Mutex::new(()),
        oauth_credentials,
        json_imports: profile_import::JsonProfileImportStore::default(),
    });
    #[cfg(debug_assertions)]
    eprintln!(
        "[startup] blocking setup completed in {} ms",
        started_at.elapsed().as_millis()
    );
    Ok(())
}

fn start_post_startup_tasks(app_handle: &tauri::AppHandle) {
    let state = app_handle.state::<AppState>();
    let repository = state.repository.clone();
    let secrets = state.secrets.clone();
    let collaboration = state.collaboration.clone();
    tauri::async_runtime::spawn(async move {
        #[cfg(debug_assertions)]
        let started_at = Instant::now();
        if let Err(error) = post_startup_initialise(repository, secrets, collaboration).await {
            eprintln!("[startup] background initialisation failed: {error}");
        }
        #[cfg(debug_assertions)]
        eprintln!(
            "[startup] background services ready in {} ms",
            started_at.elapsed().as_millis()
        );
    });
}

async fn post_startup_initialise(
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    collaboration: Arc<CollaborationManager>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    purge_legacy_channels(&repository, secrets).await?;
    repository
        .ensure_local_secret_store_backend()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    collaboration.spawn_enabled_connectors();
    Ok(())
}

async fn purge_legacy_channels(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let references = repository
        .legacy_channel_secret_refs()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    for reference in references {
        secrets
            .delete(&reference)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    repository
        .drop_legacy_channels()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(())
}

pub fn read_relay_gateway_token(reference: &str, data_dir: &Path) -> Result<String, String> {
    if !(reference.starts_with("client-key:") || reference.starts_with("profile:")) {
        return Err("invalid secret reference".to_owned());
    }
    let store = LocalEncryptedSecretStore::open(data_dir).map_err(|error| error.to_string())?;
    store
        .get_blocking(reference)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use crate::{
        collaboration::CollaborationManager,
        database::Repository,
        gateway::GatewayManager,
        oauth_credentials::OAuthCredentialStore,
        secrets::{LocalEncryptedSecretStore, MemorySecretStore, SecretStore},
    };

    #[test]
    fn installs_a_process_default_rustls_provider() {
        super::install_rustls_provider();

        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[tokio::test]
    async fn gateway_token_cli_reads_from_the_local_vault() {
        let data_dir =
            std::env::temp_dir().join(format!("codex-relay-cli-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = Arc::new(LocalEncryptedSecretStore::open(&data_dir).unwrap());
        store.set("client-key:one", "crl_secret").await.unwrap();

        assert_eq!(
            super::read_relay_gateway_token("client-key:one", &data_dir).unwrap(),
            "crl_secret"
        );
        assert!(super::read_relay_gateway_token("invalid:one", &data_dir).is_err());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn post_startup_initialisation_finishes_after_state_is_available() {
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        let oauth_credentials = Arc::new(OAuthCredentialStore::new(secrets.clone()));
        let gateway = Arc::new(GatewayManager::new(
            repository.clone(),
            secrets.clone(),
            oauth_credentials.clone(),
            PathBuf::from("/tmp/codex-relay-post-startup-certs"),
        ));
        let collaboration = Arc::new(CollaborationManager::new(
            repository.clone(),
            secrets.clone(),
            oauth_credentials,
            gateway,
            PathBuf::from("/tmp/codex-relay-post-startup-test"),
        ));

        super::post_startup_initialise(repository.clone(), secrets, collaboration)
            .await
            .unwrap();

        assert!(!repository.ensure_local_secret_store_backend().unwrap());
    }
}
