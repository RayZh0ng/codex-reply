mod codex_runtime;
mod commands;
mod database;
mod domain;
mod error;
mod gateway;
mod notifications;
mod profiles;
mod secrets;

use std::{error::Error, sync::Arc};

#[cfg(target_os = "macos")]
use std::process::Command;

use commands::AppState;
use database::Repository;
use gateway::GatewayManager;
use secrets::{KeyringSecretStore, SecretStore};
use tauri::Manager;

#[cfg(target_os = "macos")]
const EXPECTED_MACOS_TEAM_IDENTIFIER: &str = "89DX2475C3";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_rustls_provider();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| initialise(app))
        .invoke_handler(tauri::generate_handler![
            commands::dashboard_snapshot,
            commands::list_profiles,
            commands::create_profile,
            commands::update_profile,
            commands::sync_profile_account_info,
            commands::refresh_profile_quotas,
            commands::delete_profile,
            commands::select_current_profile,
            commands::desktop_workspace_settings,
            commands::update_desktop_workspace_settings,
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
            commands::list_client_keys,
            commands::create_client_key,
            commands::revoke_client_key,
            commands::list_channels,
            commands::upsert_channel,
            commands::test_channel,
            commands::delete_channel,
            commands::start_oauth_import,
            commands::oauth_import_status,
            commands::cancel_oauth_import,
            commands::complete_oauth_import
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

fn install_rustls_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

fn initialise(app: &tauri::App) -> Result<(), Box<dyn Error>> {
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let repository = Arc::new(
        Repository::open(&data_dir.join("relay.sqlite"))
            .map_err(|error| std::io::Error::other(error.to_string()))?,
    );
    let secrets: Arc<dyn SecretStore> = Arc::new(KeyringSecretStore::new());
    let gateway = Arc::new(GatewayManager::new(
        repository.clone(),
        secrets.clone(),
        data_dir.join("certs"),
    ));
    let runtime = Arc::new(codex_runtime::CodexRuntime::new(
        data_dir.clone(),
        secrets.clone(),
    ));
    tauri::async_runtime::block_on(profiles::migrate_oauth_credentials(
        &repository,
        has_stable_macos_signature(),
    ));
    app.manage(AppState {
        repository,
        secrets,
        gateway,
        runtime,
        quota_refresh_lock: tokio::sync::Mutex::new(()),
        oauth_credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    Ok(())
}

fn has_stable_macos_signature() -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(executable) = std::env::current_exe() else {
            return false;
        };
        let Ok(output) = Command::new("codesign")
            .args(["-dvvv"])
            .arg(executable)
            .output()
        else {
            return false;
        };
        signed_by_expected_macos_team(&String::from_utf8_lossy(&output.stderr))
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn signed_by_expected_macos_team(details: &str) -> bool {
    details.contains(&format!("TeamIdentifier={EXPECTED_MACOS_TEAM_IDENTIFIER}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn installs_a_process_default_rustls_provider() {
        super::install_rustls_provider();

        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn accepts_only_the_expected_macos_signing_team() {
        assert!(super::signed_by_expected_macos_team(
            "Authority=Apple Development\nTeamIdentifier=89DX2475C3"
        ));
        assert!(!super::signed_by_expected_macos_team(
            "Authority=Apple Development\nTeamIdentifier=not set"
        ));
    }
}
