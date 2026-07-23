use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::DateTime;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::{
    database::StoredProfile,
    domain::{
        CurrentProfileActivation, DesktopWorkspaceMode, ManagedTaskStatus, OAuthImportStatus,
        ProfileKind, ProfileQuota, ProfileQuotaBucket, ProfileQuotaWindow, ProfileSubscription,
        StartManagedTaskInput,
    },
    error::{AppError, AppResult},
    profiles::CodexOAuthCredential,
    secrets::SecretStore,
};

const MANAGED_TASK_ARGS: [&str; 5] = ["exec", "--ephemeral", "--sandbox", "workspace-write", "-"];
const DESKTOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const DESKTOP_QUIT_TIMEOUT: Duration = Duration::from_secs(5);
const DESKTOP_APP_CANDIDATES: [&str; 2] = ["ChatGPT", "Codex"];
const CODEX_KEYCHAIN_SERVICE: &str = "Codex Auth";
const OAUTH_CALLBACK_PORT: u16 = 1455;
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const RATE_LIMIT_READ_TIMEOUT: Duration = Duration::from_secs(12);
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CHATGPT_ACCOUNT_URL: &str = "https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27";

pub(crate) trait RuntimeProcess: Send {
    fn try_wait(&mut self) -> AppResult<Option<bool>>;
    fn terminate(&mut self);
}

struct SystemProcess(Child);
impl RuntimeProcess for SystemProcess {
    fn try_wait(&mut self) -> AppResult<Option<bool>> {
        self.0
            .try_wait()
            .map(|status| status.map(|value| value.success()))
            .map_err(|_| AppError::RuntimeUnavailable)
    }
    fn terminate(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

trait CodexRunner: Send + Sync {
    fn start_task(
        &self,
        home: &Path,
        working_directory: &Path,
        instruction: &str,
    ) -> AppResult<Box<dyn RuntimeProcess>>;
}
struct SystemCodexRunner;
impl CodexRunner for SystemCodexRunner {
    fn start_task(
        &self,
        home: &Path,
        working_directory: &Path,
        instruction: &str,
    ) -> AppResult<Box<dyn RuntimeProcess>> {
        let mut command = Command::new("codex");
        command
            .args(MANAGED_TASK_ARGS)
            .env("CODEX_HOME", home)
            .current_dir(working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|_| AppError::RuntimeUnavailable)?;
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            return Err(AppError::RuntimeUnavailable);
        };
        if stdin.write_all(instruction.as_bytes()).is_err() || stdin.flush().is_err() {
            let _ = child.kill();
            return Err(AppError::RuntimeUnavailable);
        }
        Ok(Box::new(SystemProcess(child)))
    }
}

trait DesktopController: Send + Sync {
    fn launch(&self, codex_home: &Path, user_data_dir: Option<&Path>) -> AppResult<()>;
    fn quit_running(&self) -> AppResult<()>;
}

trait DesktopCredentialStore: Send + Sync {
    fn project(&self, codex_home: &Path, auth_json: &str) -> AppResult<()>;
}

trait BrowserLauncher: Send + Sync {
    fn open(&self, url: &str) -> AppResult<()>;
}

struct SystemBrowserLauncher;

impl BrowserLauncher for SystemBrowserLauncher {
    fn open(&self, url: &str) -> AppResult<()> {
        #[cfg(target_os = "macos")]
        {
            let status = Command::new("open")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|_| AppError::RuntimeUnavailable)?;
            status
                .success()
                .then_some(())
                .ok_or(AppError::RuntimeUnavailable)
        }
        #[cfg(target_os = "windows")]
        {
            let status = Command::new("explorer.exe")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|_| AppError::RuntimeUnavailable)?;
            status
                .success()
                .then_some(())
                .ok_or(AppError::RuntimeUnavailable)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = url;
            Err(AppError::RuntimeUnavailable)
        }
    }
}

struct SystemDesktopController;
impl DesktopController for SystemDesktopController {
    fn launch(&self, codex_home: &Path, user_data_dir: Option<&Path>) -> AppResult<()> {
        #[cfg(target_os = "macos")]
        {
            launch_macos_desktop(codex_home, user_data_dir)
        }
        #[cfg(target_os = "windows")]
        {
            launch_windows_desktop(codex_home, user_data_dir)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (codex_home, user_data_dir);
            Err(AppError::DesktopUnavailable)
        }
    }

    fn quit_running(&self) -> AppResult<()> {
        #[cfg(target_os = "macos")]
        {
            quit_macos_desktop()
        }
        #[cfg(target_os = "windows")]
        {
            quit_windows_desktop()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(AppError::DesktopUnavailable)
        }
    }
}

struct SystemDesktopCredentialStore;
impl DesktopCredentialStore for SystemDesktopCredentialStore {
    fn project(&self, codex_home: &Path, auth_json: &str) -> AppResult<()> {
        #[cfg(target_os = "macos")]
        {
            write_macos_codex_keychain(codex_home, auth_json)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (codex_home, auth_json);
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn launch_macos_desktop(codex_home: &Path, user_data_dir: Option<&Path>) -> AppResult<()> {
    DESKTOP_APP_CANDIDATES
        .iter()
        .copied()
        .find_map(|app| open_macos_desktop_app(app, codex_home, user_data_dir).ok())
        .ok_or(AppError::DesktopUnavailable)
}

#[cfg(target_os = "macos")]
fn open_macos_desktop_app(
    app: &str,
    codex_home: &Path,
    user_data_dir: Option<&Path>,
) -> AppResult<()> {
    let mut command = Command::new("open");
    command
        .args(macos_desktop_launch_args(app, codex_home, user_data_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_desktop_command(&mut command)
}

#[cfg(target_os = "macos")]
fn macos_desktop_launch_args(
    app: &str,
    codex_home: &Path,
    user_data_dir: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "-n".into(),
        "-a".into(),
        app.into(),
        "--env".into(),
        format!("CODEX_HOME={}", codex_home.display()),
    ];
    if let Some(user_data_dir) = user_data_dir {
        args.extend([
            "--env".into(),
            format!("CODEX_ELECTRON_USER_DATA_PATH={}", user_data_dir.display()),
            "--args".into(),
            format!("--user-data-dir={}", user_data_dir.display()),
        ]);
    }
    args
}

#[cfg(target_os = "macos")]
fn quit_macos_desktop() -> AppResult<()> {
    for app in DESKTOP_APP_CANDIDATES {
        let _ = Command::new("osascript")
            .args(["-e", &format!("tell application \"{app}\" to quit")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    wait_for_desktop_exit()
}

#[cfg(target_os = "macos")]
fn desktop_processes_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "ChatGPT"])
        .status()
        .is_ok_and(|status| status.success())
        || Command::new("pgrep")
            .args(["-x", "Codex"])
            .status()
            .is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn wait_for_desktop_exit() -> AppResult<()> {
    let deadline = Instant::now() + DESKTOP_QUIT_TIMEOUT;
    while desktop_processes_running() {
        if Instant::now() >= deadline {
            return Err(AppError::DesktopUnavailable);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_desktop_command(command: &mut Command) -> AppResult<()> {
    run_command_with_timeout(command, DESKTOP_COMMAND_TIMEOUT)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_command_with_timeout(command: &mut Command, timeout: Duration) -> AppResult<()> {
    let mut child = command.spawn().map_err(|_| AppError::DesktopUnavailable)?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) | Err(_) => return Err(AppError::DesktopUnavailable),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::DesktopUnavailable);
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[cfg(target_os = "windows")]
fn launch_windows_desktop(codex_home: &Path, user_data_dir: Option<&Path>) -> AppResult<()> {
    let executable = find_windows_desktop_executable().ok_or(AppError::DesktopUnavailable)?;
    let mut command = windows_desktop_command(&executable, codex_home, user_data_dir);
    command.spawn().map_err(|_| AppError::DesktopUnavailable)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_desktop_command(
    executable: &Path,
    codex_home: &Path,
    user_data_dir: Option<&Path>,
) -> Command {
    let mut command = Command::new(executable);
    command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(user_data_dir) = user_data_dir {
        command
            .env("CODEX_ELECTRON_USER_DATA_PATH", user_data_dir)
            .arg(format!("--user-data-dir={}", user_data_dir.display()));
    }
    command
}

#[cfg(target_os = "windows")]
fn quit_windows_desktop() -> AppResult<()> {
    let script = r#"
$processes = Get-Process -Name ChatGPT,Codex -ErrorAction SilentlyContinue
foreach ($process in $processes) { [void]$process.CloseMainWindow() }
$deadline = (Get-Date).AddSeconds(5)
while ((Get-Process -Name ChatGPT,Codex -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
  Start-Sleep -Milliseconds 100
}
if (Get-Process -Name ChatGPT,Codex -ErrorAction SilentlyContinue) { exit 1 }
exit 0
"#;
    powershell_command(script)
        .status()
        .map_err(|_| AppError::DesktopUnavailable)?
        .success()
        .then_some(())
        .ok_or(AppError::DesktopUnavailable)
}

#[cfg(target_os = "windows")]
fn powershell_command(script: &str) -> Command {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(target_os = "windows")]
fn find_windows_desktop_executable() -> Option<PathBuf> {
    let script = r#"
$running = Get-Process -Name ChatGPT,Codex -ErrorAction SilentlyContinue |
  Where-Object { -not [string]::IsNullOrWhiteSpace($_.Path) } |
  Select-Object -First 1 -ExpandProperty Path
if ($running) { Write-Output $running; exit 0 }
$packages = Get-AppxPackage | Where-Object {
  $_.Name -like 'OpenAI.ChatGPT*' -or $_.Name -like 'OpenAI.Codex*'
} | Sort-Object @{ Expression = { if ($_.Name -like 'OpenAI.ChatGPT*') { 0 } else { 1 } } }, @{ Expression = { $_.Version }; Descending = $true }
foreach ($package in $packages) {
  $candidate = Get-ChildItem -Path $package.InstallLocation -Filter ChatGPT.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
  if (-not $candidate) { $candidate = Get-ChildItem -Path $package.InstallLocation -Filter Codex.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName }
  if ($candidate) { Write-Output $candidate; exit 0 }
}
$paths = @("$env:LOCALAPPDATA\\Programs\\ChatGPT\\ChatGPT.exe", "$env:LOCALAPPDATA\\Programs\\OpenAI ChatGPT\\ChatGPT.exe")
foreach ($candidate in $paths) { if (Test-Path -LiteralPath $candidate) { Write-Output $candidate; exit 0 } }
exit 1
"#;
    let output = powershell_command(script).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn write_macos_codex_keychain(codex_home: &Path, auth_json: &str) -> AppResult<()> {
    let entry = keyring::Entry::new(CODEX_KEYCHAIN_SERVICE, &codex_keychain_account(codex_home))
        .map_err(|_| AppError::CodexKeychainUnavailable)?;
    entry
        .set_password(auth_json)
        .map_err(|_| AppError::CodexKeychainUnavailable)
}

fn codex_keychain_account(codex_home: &Path) -> String {
    let resolved_home = fs::canonicalize(codex_home).unwrap_or_else(|_| codex_home.to_path_buf());
    let digest = Sha256::digest(resolved_home.to_string_lossy().as_bytes());
    format!("cli|{}", &format!("{digest:x}")[..16])
}

struct ManagedTask {
    profile_id: String,
    process: Box<dyn RuntimeProcess>,
}
#[derive(Clone, Copy)]
enum ManagedTaskPhase {
    Idle,
    Running,
    Completed,
    Failed,
    Cancelled,
}
struct ManagedTaskRegistry {
    active: Option<ManagedTask>,
    last_phase: ManagedTaskPhase,
    last_profile_id: Option<String>,
}
impl Default for ManagedTaskRegistry {
    fn default() -> Self {
        Self {
            active: None,
            last_phase: ManagedTaskPhase::Idle,
            last_profile_id: None,
        }
    }
}

struct OAuthAttempt {
    profile_id: Option<String>,
    phase: AttemptPhase,
    credential: Option<CodexOAuthCredential>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptPhase {
    Authorizing,
    Authenticated,
    Failed,
    Cancelled,
}
struct CurrentProfileAttempt {
    id: String,
    profile_id: String,
    phase: CurrentProfileAttemptPhase,
    workspace_mode: DesktopWorkspaceMode,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CurrentProfileAttemptPhase {
    Switching,
    Activated,
    AuthFileWriteFailed,
    CodexKeychainWriteFailed,
    DesktopRestartFailed,
    Failed,
}

pub struct CompletedOAuthImport {
    pub profile_id: Option<String>,
    pub credential: CodexOAuthCredential,
}

#[derive(Debug, Clone)]
pub enum DesktopWorkspaceLaunch {
    Fresh(String),
    PerProfile,
    Shared,
}

impl DesktopWorkspaceLaunch {
    fn mode(&self) -> DesktopWorkspaceMode {
        match self {
            Self::Fresh(_) => DesktopWorkspaceMode::Fresh,
            Self::PerProfile => DesktopWorkspaceMode::PerProfile,
            Self::Shared => DesktopWorkspaceMode::Shared,
        }
    }
}

#[derive(Debug, Clone)]
struct AppServerAccountSnapshot {
    quota: ProfileQuota,
    email: Option<String>,
    plan_type: Option<String>,
}

fn read_rate_limits_from_app_server(home: &Path) -> AppResult<AppServerAccountSnapshot> {
    let mut command = Command::new("codex");
    command
        .args(["app-server", "--stdio"])
        .env("CODEX_HOME", home)
        .env_remove("CODEX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| AppError::RuntimeUnavailable)?;
    let result = (|| -> AppResult<AppServerAccountSnapshot> {
        let mut stdin = child.stdin.take().ok_or(AppError::RuntimeUnavailable)?;
        for request in [
            serde_json::json!({
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
            serde_json::json!({"method": "initialized", "params": {}}),
            serde_json::json!({"method": "account/read", "id": 2, "params": {"refreshToken": true}}),
            serde_json::json!({"method": "account/rateLimits/read", "id": 3}),
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
        let deadline = Instant::now() + RATE_LIMIT_READ_TIMEOUT;
        let mut account_seen = false;
        let mut email = None;
        let mut plan_type = None;
        let mut quota = None;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let line = receiver
                .recv_timeout(remaining)
                .map_err(|_| AppError::RuntimeUnavailable)?;
            let response: serde_json::Value =
                serde_json::from_str(&line).map_err(|_| AppError::RuntimeUnavailable)?;
            if response.get("id") == Some(&serde_json::json!(2)) {
                account_seen = true;
                if let Some(result) = response.get("result") {
                    let (next_email, next_plan_type) = parse_app_server_account(result);
                    email = next_email;
                    plan_type = next_plan_type;
                }
            }
            if response.get("id") == Some(&serde_json::json!(3)) {
                quota = Some(parse_rate_limits_response(response)?);
            }
            if account_seen {
                if let Some(quota) = quota {
                    return Ok(AppServerAccountSnapshot {
                        plan_type: plan_type.or_else(|| {
                            quota
                                .buckets
                                .iter()
                                .find_map(|bucket| bucket.plan_type.clone())
                        }),
                        email,
                        quota,
                    });
                }
            }
        }
        if let Some(quota) = quota {
            return Ok(AppServerAccountSnapshot {
                plan_type: plan_type.or_else(|| {
                    quota
                        .buckets
                        .iter()
                        .find_map(|bucket| bucket.plan_type.clone())
                }),
                email,
                quota,
            });
        }
        Err(AppError::RuntimeUnavailable)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitResponse {
    rate_limits: RateLimits,
    #[serde(default)]
    rate_limits_by_limit_id: Option<HashMap<String, RateLimits>>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    limit_id: Option<String>,
    limit_name: Option<String>,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    used_percent: f64,
    window_duration_mins: i64,
    resets_at: Option<i64>,
}

fn parse_rate_limits_response(response: serde_json::Value) -> AppResult<ProfileQuota> {
    let result = response
        .get("result")
        .ok_or(AppError::RuntimeUnavailable)?
        .clone();
    let response: RateLimitResponse =
        serde_json::from_value(result).map_err(|_| AppError::RuntimeUnavailable)?;
    let primary = response.rate_limits.primary.clone().map(rate_limit_window);
    let secondary = response
        .rate_limits
        .secondary
        .clone()
        .map(rate_limit_window);
    let mut buckets = response
        .rate_limits_by_limit_id
        .unwrap_or_default()
        .into_iter()
        .map(|(id, limits)| rate_limit_bucket(Some(id), limits))
        .collect::<Vec<_>>();
    if buckets.is_empty() {
        buckets.push(rate_limit_bucket(None, response.rate_limits.clone()));
    }
    let has_window = primary.is_some()
        || secondary.is_some()
        || buckets
            .iter()
            .any(|bucket| bucket.primary.is_some() || bucket.secondary.is_some());
    Ok(ProfileQuota {
        status: if has_window {
            "available"
        } else {
            "unavailable"
        }
        .to_owned(),
        message: if has_window {
            "已通过本机 Codex 运行时同步额度。".to_owned()
        } else {
            "当前账号未返回可用额度窗口。".to_owned()
        },
        source: has_window.then(|| "app_server".to_owned()),
        synced_at_ms: has_window.then(now_ms),
        last_attempt_at_ms: now_ms(),
        last_error: None,
        primary,
        secondary,
        buckets,
        rate_limit_reached_type: response.rate_limits.rate_limit_reached_type,
    })
}

fn rate_limit_bucket(fallback_id: Option<String>, limits: RateLimits) -> ProfileQuotaBucket {
    ProfileQuotaBucket {
        id: limits.limit_id.or(fallback_id),
        name: limits.limit_name,
        plan_type: limits.plan_type,
        primary: limits.primary.map(rate_limit_window),
        secondary: limits.secondary.map(rate_limit_window),
    }
}

fn parse_app_server_account(value: &serde_json::Value) -> (Option<String>, Option<String>) {
    let account = value.get("account");
    if account
        .and_then(|account| account.get("type"))
        .and_then(|value| value.as_str())
        != Some("chatgpt")
    {
        return (None, None);
    }
    (
        account
            .and_then(|account| account.get("email"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        account
            .and_then(|account| account.get("planType"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
    )
}

#[derive(Deserialize)]
struct UsageResponse {
    #[serde(rename = "rate_limit")]
    rate_limit: Option<UsageRateLimit>,
}

#[derive(Deserialize)]
struct UsageRateLimit {
    #[serde(rename = "primary_window")]
    primary_window: Option<UsageRateWindow>,
    #[serde(rename = "secondary_window")]
    secondary_window: Option<UsageRateWindow>,
    #[serde(rename = "limit_reached")]
    limit_reached: Option<bool>,
}

#[derive(Deserialize)]
struct UsageRateWindow {
    #[serde(rename = "used_percent")]
    used_percent: Option<f64>,
    #[serde(rename = "limit_window_seconds")]
    limit_window_seconds: Option<i64>,
    #[serde(rename = "reset_after_seconds")]
    reset_after_seconds: Option<i64>,
    #[serde(rename = "reset_at")]
    reset_at: Option<i64>,
}

#[derive(Debug)]
enum ChatgptApiError {
    Unauthorized,
    Unavailable,
}

async fn read_rate_limits_from_usage_api(
    credential: &CodexOAuthCredential,
) -> Result<ProfileQuota, ChatgptApiError> {
    let usage: UsageResponse = chatgpt_get_json(CHATGPT_USAGE_URL, credential).await?;
    let rate_limit = usage.rate_limit.ok_or(ChatgptApiError::Unavailable)?;
    let primary = rate_limit.primary_window.map(usage_rate_limit_window);
    let secondary = rate_limit.secondary_window.map(usage_rate_limit_window);
    let has_window = primary.is_some() || secondary.is_some();
    Ok(ProfileQuota {
        status: if has_window {
            "available"
        } else {
            "unavailable"
        }
        .to_owned(),
        message: if has_window {
            "额度已同步。"
        } else {
            "当前账号未返回可用额度窗口。"
        }
        .to_owned(),
        source: has_window.then(|| "usage_api".to_owned()),
        synced_at_ms: has_window.then(now_ms),
        last_attempt_at_ms: now_ms(),
        last_error: None,
        primary,
        secondary,
        buckets: Vec::new(),
        rate_limit_reached_type: rate_limit
            .limit_reached
            .filter(|reached| *reached)
            .map(|_| "usage_limit".to_owned()),
    })
}

#[derive(Deserialize)]
struct AccountCheckResponse {
    accounts: Option<AccountCheckAccounts>,
}

#[derive(Deserialize)]
struct AccountCheckAccounts {
    default: Option<AccountCheckDefault>,
}

#[derive(Deserialize)]
struct AccountCheckDefault {
    account: Option<AccountCheckAccount>,
    entitlement: Option<AccountEntitlement>,
    last_active_subscription: Option<AccountLastActiveSubscription>,
}

#[derive(Deserialize)]
struct AccountCheckAccount {
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct AccountEntitlement {
    subscription_plan: Option<String>,
    expires_at: Option<serde_json::Value>,
    will_renew: Option<bool>,
}

#[derive(Deserialize)]
struct AccountLastActiveSubscription {
    will_renew: Option<bool>,
}

#[derive(Debug, Clone)]
struct AccountCheckSnapshot {
    account_id: Option<String>,
    subscription: ProfileSubscription,
}

async fn read_subscription_from_account_api(
    credential: &CodexOAuthCredential,
) -> Result<AccountCheckSnapshot, ChatgptApiError> {
    let response: AccountCheckResponse = chatgpt_get_json(CHATGPT_ACCOUNT_URL, credential).await?;
    Ok(account_check_snapshot(response))
}

fn account_check_snapshot(response: AccountCheckResponse) -> AccountCheckSnapshot {
    let default = response.accounts.and_then(|accounts| accounts.default);
    let entitlement = default
        .as_ref()
        .and_then(|account| account.entitlement.as_ref());
    let plan_type = normalize_plan_type(
        entitlement.and_then(|entitlement| entitlement.subscription_plan.clone()),
    );
    let period_ends_at_ms = entitlement
        .and_then(|entitlement| entitlement.expires_at.as_ref())
        .and_then(parse_period_end_ms);
    let will_renew = entitlement
        .and_then(|entitlement| entitlement.will_renew)
        .or_else(|| {
            default
                .as_ref()
                .and_then(|account| account.last_active_subscription.as_ref())
                .and_then(|subscription| subscription.will_renew)
        });
    let available = plan_type.is_some() || period_ends_at_ms.is_some();
    AccountCheckSnapshot {
        account_id: default
            .as_ref()
            .and_then(|account| account.account.as_ref())
            .and_then(|account| account.account_id.clone()),
        subscription: ProfileSubscription {
            status: if available {
                "available"
            } else {
                "unavailable"
            }
            .to_owned(),
            plan_type,
            period_ends_at_ms,
            will_renew,
            source: available.then(|| "account_check".to_owned()),
            synced_at_ms: available.then(now_ms),
            last_attempt_at_ms: now_ms(),
            last_error: None,
        },
    }
}

async fn chatgpt_get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    credential: &CodexOAuthCredential,
) -> Result<T, ChatgptApiError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| ChatgptApiError::Unavailable)?;
    for attempt in 0..3 {
        let mut request = client
            .get(url)
            .bearer_auth(&credential.access_token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(account_id) = credential
            .account_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                return response
                    .json()
                    .await
                    .map_err(|_| ChatgptApiError::Unavailable)
            }
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                return Err(ChatgptApiError::Unauthorized)
            }
            Ok(response)
                if (response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || response.status().is_server_error())
                    && attempt < 2 => {}
            Ok(_) | Err(_) if attempt == 2 => return Err(ChatgptApiError::Unavailable),
            Ok(_) | Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(250_u64 * (1_u64 << attempt))).await;
    }
    Err(ChatgptApiError::Unavailable)
}

fn parse_period_end_ms(value: &serde_json::Value) -> Option<i64> {
    if let Some(seconds_or_ms) = value.as_i64() {
        return if seconds_or_ms > 1_000_000_000_000 {
            Some(seconds_or_ms)
        } else {
            seconds_or_ms.checked_mul(1000)
        };
    }
    value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
}

fn subscription_from_app_server(plan_type: Option<String>) -> ProfileSubscription {
    let available = plan_type.is_some();
    ProfileSubscription {
        status: if available {
            "available"
        } else {
            "unavailable"
        }
        .to_owned(),
        plan_type,
        period_ends_at_ms: None,
        will_renew: None,
        source: available.then(|| "app_server".to_owned()),
        synced_at_ms: available.then(now_ms),
        last_attempt_at_ms: now_ms(),
        last_error: None,
    }
}

fn unavailable_subscription() -> ProfileSubscription {
    ProfileSubscription {
        status: "unavailable".to_owned(),
        plan_type: None,
        period_ends_at_ms: None,
        will_renew: None,
        source: None,
        synced_at_ms: None,
        last_attempt_at_ms: now_ms(),
        last_error: Some("订阅资料暂不可用".to_owned()),
    }
}

fn merge_subscription(
    mut primary: ProfileSubscription,
    fallback: ProfileSubscription,
) -> ProfileSubscription {
    if primary.plan_type.is_none() {
        primary.plan_type = fallback.plan_type.clone();
    }
    primary.period_ends_at_ms = fallback.period_ends_at_ms;
    primary.will_renew = fallback.will_renew;
    if fallback.period_ends_at_ms.is_some() || fallback.plan_type.is_some() {
        primary.status = "available".to_owned();
        primary.source = match (primary.source.as_deref(), fallback.source.as_deref()) {
            (Some("app_server"), Some("account_check")) => {
                Some("app_server+account_check".to_owned())
            }
            (_, source) => source.map(ToOwned::to_owned).or(primary.source),
        };
        primary.synced_at_ms = fallback.synced_at_ms.or(primary.synced_at_ms);
        primary.last_error = None;
    }
    primary.last_attempt_at_ms = fallback.last_attempt_at_ms;
    primary
}

fn normalize_plan_type(plan_type: Option<String>) -> Option<String> {
    plan_type.and_then(|plan_type| {
        let normalized = plan_type.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        let normalized = match normalized.as_str() {
            "chatgptfreeplan" => "free",
            "chatgptgoplan" => "go",
            "chatgptplusplan" => "plus",
            "chatgptproplan" => "pro",
            "chatgptproliteplan" => "prolite",
            "chatgptteamplan" => "team",
            "chatgptbusinessplan" => "business",
            "chatgptenterpriseplan" => "enterprise",
            "chatgpteduplan" => "edu",
            value => value,
        };
        Some(normalized.to_owned())
    })
}

fn usage_rate_limit_window(window: UsageRateWindow) -> ProfileQuotaWindow {
    let used_percent = window.used_percent.unwrap_or_default().clamp(0.0, 100.0);
    let resets_at_ms = window
        .reset_at
        .or_else(|| {
            window
                .reset_after_seconds
                .filter(|seconds| *seconds >= 0)
                .and_then(|seconds| now_ms().checked_div(1000)?.checked_add(seconds))
        })
        .and_then(|seconds| seconds.checked_mul(1000));
    ProfileQuotaWindow {
        used_percent,
        remaining_percent: (100.0 - used_percent).max(0.0),
        window_duration_mins: window.limit_window_seconds.unwrap_or_default().max(0) / 60,
        resets_at_ms,
    }
}

pub struct QuotaRead {
    pub credential: CodexOAuthCredential,
    pub quota: ProfileQuota,
    pub subscription: ProfileSubscription,
    pub email: Option<String>,
    pub account_id: Option<String>,
}

fn rate_limit_window(window: RateLimitWindow) -> ProfileQuotaWindow {
    let used_percent = window.used_percent.clamp(0.0, 100.0);
    ProfileQuotaWindow {
        used_percent,
        remaining_percent: (100.0 - used_percent).max(0.0),
        window_duration_mins: window.window_duration_mins.max(0),
        resets_at_ms: window
            .resets_at
            .and_then(|seconds| seconds.checked_mul(1000)),
    }
}

pub struct CodexRuntime {
    root: PathBuf,
    secrets: Arc<dyn SecretStore>,
    runner: Arc<dyn CodexRunner>,
    browser: Arc<dyn BrowserLauncher>,
    desktop: Arc<dyn DesktopController>,
    desktop_credentials: Arc<dyn DesktopCredentialStore>,
    attempts: Arc<Mutex<HashMap<String, OAuthAttempt>>>,
    current_profile_attempt: Mutex<Option<CurrentProfileAttempt>>,
    auth_operation: Mutex<()>,
    managed_task: Mutex<ManagedTaskRegistry>,
}

impl CodexRuntime {
    pub fn new(root: PathBuf, secrets: Arc<dyn SecretStore>) -> Self {
        Self::with_dependencies(
            root,
            secrets,
            Arc::new(SystemCodexRunner),
            Arc::new(SystemBrowserLauncher),
            Arc::new(SystemDesktopController),
            Arc::new(SystemDesktopCredentialStore),
        )
    }

    pub async fn read_profile_rate_limits(
        &self,
        profile_id: &str,
        mut credential: CodexOAuthCredential,
    ) -> AppResult<QuotaRead> {
        if credential_needs_refresh(&credential) {
            credential = refresh_credential(&credential).await?;
        }
        let home = self.profile_home(profile_id);
        self.write_credential_to_home(&home, &credential)?;
        let mut app_result = self.read_app_server_account(&home).await;
        let mut direct_quota = if app_result.is_err() {
            Some(read_rate_limits_from_usage_api(&credential).await)
        } else {
            None
        };
        let mut direct_subscription = read_subscription_from_account_api(&credential).await;

        let direct_unauthorized = matches!(
            direct_quota.as_ref(),
            Some(Err(ChatgptApiError::Unauthorized))
        ) || matches!(
            direct_subscription.as_ref(),
            Err(ChatgptApiError::Unauthorized)
        );
        if direct_unauthorized && credential.refresh_token.is_some() {
            credential = refresh_credential(&credential).await?;
            self.write_credential_to_home(&home, &credential)?;
            if app_result.is_err() {
                app_result = self.read_app_server_account(&home).await;
            }
            direct_quota = if app_result.is_err() {
                Some(read_rate_limits_from_usage_api(&credential).await)
            } else {
                None
            };
            direct_subscription = read_subscription_from_account_api(&credential).await;
        }

        let app_snapshot = app_result.ok();
        let quota = match app_snapshot.as_ref() {
            Some(snapshot) => snapshot.quota.clone(),
            None => match direct_quota {
                Some(Ok(quota)) => quota,
                _ => return Err(AppError::UpstreamUnavailable),
            },
        };
        let mut subscription = app_snapshot
            .as_ref()
            .map(|snapshot| subscription_from_app_server(snapshot.plan_type.clone()))
            .unwrap_or_else(unavailable_subscription);
        let mut account_id = None;
        match direct_subscription {
            Ok(direct) => {
                account_id = direct.account_id.clone();
                if let Some(id) = direct.account_id {
                    credential.account_id = Some(id);
                }
                subscription = merge_subscription(subscription, direct.subscription);
            }
            Err(_) => {
                subscription.status = if subscription.plan_type.is_some() {
                    "stale"
                } else {
                    "unavailable"
                }
                .to_owned();
                subscription.last_error =
                    Some("订阅周期同步未完成，正在保留最近一次结果。".to_owned());
            }
        }

        Ok(QuotaRead {
            credential,
            quota,
            subscription,
            email: app_snapshot.and_then(|snapshot| snapshot.email),
            account_id,
        })
    }

    async fn read_app_server_account(&self, home: &Path) -> AppResult<AppServerAccountSnapshot> {
        let home = home.to_path_buf();
        tauri::async_runtime::spawn_blocking(move || read_rate_limits_from_app_server(&home))
            .await
            .unwrap_or(Err(AppError::RuntimeUnavailable))
    }
    fn with_dependencies(
        root: PathBuf,
        secrets: Arc<dyn SecretStore>,
        runner: Arc<dyn CodexRunner>,
        browser: Arc<dyn BrowserLauncher>,
        desktop: Arc<dyn DesktopController>,
        desktop_credentials: Arc<dyn DesktopCredentialStore>,
    ) -> Self {
        Self {
            root,
            secrets,
            runner,
            browser,
            desktop,
            desktop_credentials,
            attempts: Arc::new(Mutex::new(HashMap::new())),
            current_profile_attempt: Mutex::new(None),
            auth_operation: Mutex::new(()),
            managed_task: Mutex::new(ManagedTaskRegistry::default()),
        }
    }

    pub fn activate_profile(
        self: &Arc<Self>,
        profile: StoredProfile,
        credential: CodexOAuthCredential,
        workspace: DesktopWorkspaceLaunch,
    ) -> AppResult<CurrentProfileActivation> {
        self.verify_profile(&profile)?;
        let _operation = self.auth_operation.lock().map_err(|_| AppError::Internal)?;
        if self
            .current_profile_attempt
            .lock()
            .map_err(|_| AppError::Internal)?
            .as_ref()
            .is_some_and(|attempt| attempt.phase == CurrentProfileAttemptPhase::Switching)
        {
            return Err(AppError::Conflict);
        }
        let id = Uuid::new_v4().to_string();
        let profile_id = profile.profile.id.clone();
        let workspace_mode = workspace.mode();
        let credential_ref = profile
            .secret_ref
            .clone()
            .ok_or(AppError::ProfileRuntimeUnavailable)?;
        *self
            .current_profile_attempt
            .lock()
            .map_err(|_| AppError::Internal)? = Some(CurrentProfileAttempt {
            id: id.clone(),
            profile_id: profile_id.clone(),
            phase: CurrentProfileAttemptPhase::Switching,
            workspace_mode: workspace_mode.clone(),
        });
        let runtime = Arc::clone(self);
        let worker_attempt_id = id.clone();
        thread::spawn(move || {
            runtime.complete_switch(
                worker_attempt_id,
                profile_id,
                credential_ref,
                credential,
                workspace,
            )
        });
        Ok(current_profile_status(
            profile.profile.id,
            Some(id),
            CurrentProfileAttemptPhase::Switching,
            workspace_mode,
        ))
    }

    fn complete_switch(
        self: Arc<Self>,
        attempt_id: String,
        profile_id: String,
        credential_ref: String,
        mut credential: CodexOAuthCredential,
        workspace: DesktopWorkspaceLaunch,
    ) {
        let result = (|| -> AppResult<CurrentProfileAttemptPhase> {
            if credential_needs_refresh(&credential) {
                credential = tauri::async_runtime::block_on(refresh_credential(&credential))?;
                let serialized =
                    serde_json::to_string(&credential).map_err(|_| AppError::Internal)?;
                tauri::async_runtime::block_on(self.secrets.set(&credential_ref, &serialized))?;
            }
            if self.write_default_credential(&credential).is_err() {
                return Ok(CurrentProfileAttemptPhase::AuthFileWriteFailed);
            }
            self.stop_managed_task()?;
            Ok(desktop_switch_phase(self.launch_profile_desktop(
                &profile_id,
                &credential,
                &workspace,
            )))
        })();
        if let Ok(mut attempt) = self.current_profile_attempt.lock() {
            if attempt
                .as_ref()
                .is_some_and(|current| current.id == attempt_id)
            {
                if let Some(current) = attempt.as_mut() {
                    current.phase = result.unwrap_or(CurrentProfileAttemptPhase::Failed);
                }
            }
        }
    }

    pub fn current_profile_activation_status(
        &self,
        attempt_id: &str,
    ) -> AppResult<CurrentProfileActivation> {
        let attempt = self
            .current_profile_attempt
            .lock()
            .map_err(|_| AppError::Internal)?;
        let current = attempt.as_ref().ok_or(AppError::NotFound)?;
        if current.id != attempt_id {
            return Err(AppError::NotFound);
        }
        Ok(current_profile_status(
            current.profile_id.clone(),
            Some(current.id.clone()),
            current.phase,
            current.workspace_mode.clone(),
        ))
    }

    pub fn start_managed_task(
        &self,
        profile: &StoredProfile,
        credential: CodexOAuthCredential,
        input: StartManagedTaskInput,
    ) -> AppResult<ManagedTaskStatus> {
        if input.instruction.trim().is_empty() {
            return Err(AppError::ValidationFailed);
        }
        self.verify_profile(profile)?;
        let directory = PathBuf::from(input.working_directory);
        if !directory.is_absolute() || !directory.is_dir() {
            return Err(AppError::ValidationFailed);
        }
        let home = self.profile_home(&profile.profile.id);
        self.write_credential_to_home(&home, &credential)?;
        let mut task = self.managed_task.lock().map_err(|_| AppError::Internal)?;
        if Self::refresh_task(&mut task)?.is_some() {
            return Err(AppError::Conflict);
        }
        task.active = Some(ManagedTask {
            profile_id: profile.profile.id.clone(),
            process: self
                .runner
                .start_task(&home, &directory, &input.instruction)?,
        });
        task.last_phase = ManagedTaskPhase::Running;
        task.last_profile_id = Some(profile.profile.id.clone());
        Ok(task_status(
            ManagedTaskPhase::Running,
            Some(profile.profile.id.clone()),
        ))
    }
    pub fn managed_task_status(&self) -> AppResult<ManagedTaskStatus> {
        let mut task = self.managed_task.lock().map_err(|_| AppError::Internal)?;
        if let Some(id) = Self::refresh_task(&mut task)? {
            return Ok(task_status(ManagedTaskPhase::Running, Some(id)));
        }
        Ok(task_status(task.last_phase, task.last_profile_id.clone()))
    }
    pub fn cancel_managed_task(&self, confirmed: bool) -> AppResult<ManagedTaskStatus> {
        if !confirmed {
            return Err(AppError::ConfirmationRequired);
        }
        self.stop_managed_task()?;
        let task = self.managed_task.lock().map_err(|_| AppError::Internal)?;
        Ok(task_status(task.last_phase, task.last_profile_id.clone()))
    }

    pub fn start_oauth_import(&self, profile_id: Option<String>) -> AppResult<OAuthImportStatus> {
        let _operation = self.auth_operation.lock().map_err(|_| AppError::Internal)?;
        if self
            .attempts
            .lock()
            .map_err(|_| AppError::Internal)?
            .values()
            .any(|attempt| attempt.phase == AttemptPhase::Authorizing)
        {
            return Err(AppError::Conflict);
        }
        let listener = TcpListener::bind(("127.0.0.1", OAUTH_CALLBACK_PORT))
            .map_err(|_| AppError::RuntimeUnavailable)?;
        let attempt_id = Uuid::new_v4().to_string();
        let verifier = random_token();
        let state = random_token();
        let redirect = format!("http://localhost:{OAUTH_CALLBACK_PORT}/auth/callback");
        let auth_url = build_authorization_url(&redirect, &verifier, &state)?;
        self.attempts
            .lock()
            .map_err(|_| AppError::Internal)?
            .insert(
                attempt_id.clone(),
                OAuthAttempt {
                    profile_id: profile_id.clone(),
                    phase: AttemptPhase::Authorizing,
                    credential: None,
                },
            );
        if let Err(error) = self.browser.open(&auth_url) {
            self.attempts
                .lock()
                .map_err(|_| AppError::Internal)?
                .remove(&attempt_id);
            return Err(error);
        }
        let attempts = Arc::clone(&self.attempts);
        let listener_attempt_id = attempt_id.clone();
        thread::spawn(move || {
            listen_for_oauth_callback(listener, attempts, listener_attempt_id, state, verifier)
        });
        Ok(oauth_status(
            attempt_id,
            profile_id,
            AttemptPhase::Authorizing,
        ))
    }
    pub fn oauth_import_status(&self, attempt_id: &str) -> AppResult<OAuthImportStatus> {
        let attempts = self.attempts.lock().map_err(|_| AppError::Internal)?;
        let attempt = attempts.get(attempt_id).ok_or(AppError::NotFound)?;
        Ok(oauth_status(
            attempt_id.to_owned(),
            attempt.profile_id.clone(),
            attempt.phase,
        ))
    }
    pub fn complete_oauth_import(&self, attempt_id: &str) -> AppResult<CompletedOAuthImport> {
        let mut attempts = self.attempts.lock().map_err(|_| AppError::Internal)?;
        let attempt = attempts.get(attempt_id).ok_or(AppError::NotFound)?;
        if attempt.phase != AttemptPhase::Authenticated {
            return Err(AppError::ValidationFailed);
        }
        let completed = CompletedOAuthImport {
            profile_id: attempt.profile_id.clone(),
            credential: attempt.credential.clone().ok_or(AppError::Internal)?,
        };
        attempts.remove(attempt_id);
        Ok(completed)
    }
    pub fn cancel_oauth_import(&self, attempt_id: &str) -> AppResult<()> {
        let mut attempts = self.attempts.lock().map_err(|_| AppError::Internal)?;
        let attempt = attempts.get_mut(attempt_id).ok_or(AppError::NotFound)?;
        attempt.phase = AttemptPhase::Cancelled;
        Ok(())
    }
    pub fn logout_profile(&self, profile_id: &str) -> AppResult<()> {
        self.stop_managed_task_for(profile_id);
        let home = self.profile_home(profile_id);
        if home.exists() {
            fs::remove_dir_all(home).map_err(|_| AppError::RuntimeUnavailable)?
        }
        Ok(())
    }

    fn write_default_credential(&self, credential: &CodexOAuthCredential) -> AppResult<()> {
        let home = default_codex_home()?;
        self.write_credential_to_home(&home, credential)
    }
    fn write_credential_to_home(
        &self,
        home: &Path,
        credential: &CodexOAuthCredential,
    ) -> AppResult<()> {
        fs::create_dir_all(home).map_err(|_| AppError::RuntimeUnavailable)?;
        let destination = home.join("auth.json");
        let temporary = home.join(format!(".auth-{}.tmp", Uuid::new_v4()));
        fs::write(&temporary, credential.auth_json()?).map_err(|_| AppError::RuntimeUnavailable)?;
        fs::rename(temporary, destination).map_err(|_| AppError::RuntimeUnavailable)
    }
    fn launch_profile_desktop(
        &self,
        profile_id: &str,
        credential: &CodexOAuthCredential,
        workspace: &DesktopWorkspaceLaunch,
    ) -> AppResult<()> {
        let home = self.profile_home(profile_id);
        self.write_credential_to_home(&home, credential)?;
        let auth_json = credential.auth_json()?;
        self.desktop_credentials.project(&home, &auth_json)?;
        let user_data_dir = match workspace {
            DesktopWorkspaceLaunch::Fresh(id) => Some(self.fresh_desktop_user_data(id)),
            DesktopWorkspaceLaunch::PerProfile => Some(self.profile_desktop_user_data(profile_id)),
            DesktopWorkspaceLaunch::Shared => None,
        };
        if let Some(directory) = user_data_dir.as_deref() {
            fs::create_dir_all(directory).map_err(|_| AppError::RuntimeUnavailable)?;
        }
        self.desktop.launch(&home, user_data_dir.as_deref())
    }
    fn stop_managed_task(&self) -> AppResult<()> {
        let mut task = self.managed_task.lock().map_err(|_| AppError::Internal)?;
        let _ = Self::refresh_task(&mut task)?;
        if let Some(mut active) = task.active.take() {
            active.process.terminate();
            task.last_phase = ManagedTaskPhase::Cancelled;
            task.last_profile_id = Some(active.profile_id);
        }
        Ok(())
    }
    fn stop_managed_task_for(&self, profile_id: &str) {
        if let Ok(mut task) = self.managed_task.lock() {
            if task
                .active
                .as_ref()
                .is_some_and(|active| active.profile_id == profile_id)
            {
                if let Some(mut active) = task.active.take() {
                    active.process.terminate();
                    task.last_phase = ManagedTaskPhase::Cancelled;
                    task.last_profile_id = Some(active.profile_id);
                }
            }
        }
    }
    fn profile_home(&self, id: &str) -> PathBuf {
        self.root.join("runtimes").join(id)
    }
    fn profile_desktop_user_data(&self, id: &str) -> PathBuf {
        self.root
            .join("desktop-instances")
            .join(id)
            .join("electron")
    }
    fn fresh_desktop_user_data(&self, id: &str) -> PathBuf {
        self.root
            .join("desktop-instances")
            .join("fresh")
            .join(id)
            .join("electron")
    }
    pub fn quit_desktop_for_shared_switch(&self) -> AppResult<()> {
        self.desktop.quit_running()
    }
    pub fn delete_fresh_desktop_workspace(&self, id: &str) -> AppResult<()> {
        Uuid::parse_str(id).map_err(|_| AppError::ValidationFailed)?;
        let workspace_root = self.root.join("desktop-instances").join("fresh").join(id);
        match fs::remove_dir_all(workspace_root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AppError::RuntimeUnavailable),
        }
    }
    fn verify_profile(&self, profile: &StoredProfile) -> AppResult<()> {
        if profile.profile.kind != ProfileKind::CodexOauth
            || !profile.profile.enabled
            || !profile.profile.credential_configured
            || profile.secret_ref.is_none()
        {
            Err(AppError::ProfileRuntimeUnavailable)
        } else {
            Ok(())
        }
    }
    fn refresh_task(task: &mut ManagedTaskRegistry) -> AppResult<Option<String>> {
        let Some(active) = task.active.as_mut() else {
            return Ok(None);
        };
        match active.process.try_wait()? {
            None => Ok(Some(active.profile_id.clone())),
            Some(success) => {
                let id = active.profile_id.clone();
                task.active = None;
                task.last_phase = if success {
                    ManagedTaskPhase::Completed
                } else {
                    ManagedTaskPhase::Failed
                };
                task.last_profile_id = Some(id);
                Ok(None)
            }
        }
    }
}

fn listen_for_oauth_callback(
    listener: TcpListener,
    attempts: Arc<Mutex<HashMap<String, OAuthAttempt>>>,
    attempt_id: String,
    expected_state: String,
    verifier: String,
) {
    let _ = listener.set_nonblocking(true);
    let deadline = SystemTime::now() + Duration::from_secs(300);
    let mut accepted = None;
    while SystemTime::now() < deadline {
        match listener.accept() {
            Ok(connection) => {
                accepted = Some(connection);
                break;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if attempts
                    .lock()
                    .ok()
                    .and_then(|attempts| attempts.get(&attempt_id).map(|attempt| attempt.phase))
                    != Some(AttemptPhase::Authorizing)
                {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return,
        }
    }
    let Some((mut stream, _)) = accepted else {
        if let Ok(mut attempts) = attempts.lock() {
            if let Some(attempt) = attempts.get_mut(&attempt_id) {
                attempt.phase = AttemptPhase::Failed;
            }
        }
        return;
    };
    let mut request = String::new();
    let _ = stream.read_to_string(&mut request);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let url = Url::parse(&format!("http://localhost{path}"));
    let valid = url.as_ref().ok().is_some_and(|url| {
        url.path() == "/auth/callback"
            && url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .is_some_and(|(_, value)| value == expected_state)
    });
    let code = url.ok().and_then(|url| {
        url.query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string())
    });
    let response = oauth_callback_response(valid && code.is_some());
    let _ = stream.write_all(response.as_bytes());
    let outcome = code
        .filter(|_| valid)
        .map(|code| tauri::async_runtime::block_on(exchange_code(&code, &verifier)));
    if let Ok(mut guard) = attempts.lock() {
        if let Some(attempt) = guard.get_mut(&attempt_id) {
            match outcome {
                Some(Ok(credential)) => {
                    attempt.phase = AttemptPhase::Authenticated;
                    attempt.credential = Some(credential);
                }
                _ => attempt.phase = AttemptPhase::Failed,
            }
        }
    }
}

const OAUTH_CALLBACK_PAGE_STYLE: &str = r#"
<style>
:root {
  color: #182235;
  background: #f5f7fb;
  font-family: "SF Pro Display", "SF Pro Text", -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  font-synthesis: none;
  text-rendering: optimizeLegibility;
  -webkit-font-smoothing: antialiased;
}
* { box-sizing: border-box; }
body {
  display: grid;
  min-width: 320px;
  min-height: 100vh;
  margin: 0;
  place-items: center;
  background:
    radial-gradient(circle at 50% 0%, rgb(220 237 255 / 82%), transparent 39rem),
    #f5f7fb;
}
main { width: min(100% - 40px, 448px); }
.card {
  padding: 42px 38px 36px;
  border: 1px solid rgb(222 229 239 / 92%);
  border-radius: 24px;
  background: rgb(255 255 255 / 92%);
  box-shadow: 0 24px 65px rgb(45 78 120 / 13%);
  text-align: center;
}
.brand {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 30px;
  color: #526075;
  font-size: 13px;
  font-weight: 700;
}
.brand-mark {
  display: grid;
  width: 24px;
  height: 24px;
  border-radius: 8px;
  place-items: center;
  background: linear-gradient(135deg, #2e8cf2, #176fd4);
  box-shadow: 0 6px 14px rgb(47 113 219 / 24%);
  color: white;
  font-size: 11px;
  font-weight: 800;
  letter-spacing: -0.08em;
}
.status-icon {
  display: grid;
  width: 58px;
  height: 58px;
  margin: 0 auto 21px;
  border-radius: 50%;
  place-items: center;
}
.status-icon.success { background: #e5f4ec; color: #16815d; }
.status-icon.error { background: #fff0ed; color: #d45f4d; }
.status-icon svg { width: 30px; height: 30px; }
h1 {
  margin: 0;
  color: #182235;
  font-size: 26px;
  letter-spacing: -0.045em;
  line-height: 1.2;
}
p {
  margin: 12px 0 0;
  color: #69758a;
  font-size: 14px;
  line-height: 1.65;
}
.next-step {
  margin-top: 27px;
  padding: 13px 16px;
  border-radius: 12px;
  background: #f4f7fb;
  color: #526075;
  font-size: 13px;
  line-height: 1.55;
}
@media (max-width: 440px) {
  main { width: min(100% - 28px, 448px); }
  .card { padding: 34px 24px 28px; border-radius: 20px; }
}
</style>
"#;

fn oauth_callback_response(success: bool) -> String {
    let (status_line, status_class, icon, heading, message, next_step) = if success {
        (
            "200 OK",
            "success",
            r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 4.25 4.25L19 6.5"/></svg>"#,
            "授权完成",
            "你的 OpenAI 账号已授权给 Codex Relay。",
            "现在可以安全关闭此页面，并返回 Codex Relay 继续。",
        )
    } else {
        (
            "400 Bad Request",
            "error",
            r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8v4"/><path d="M12 16h.01"/><path d="M10.28 3.86 2.65 17.5A2 2 0 0 0 4.4 20.5h15.2a2 2 0 0 0 1.75-3L13.72 3.86a2 2 0 0 0-3.44 0Z"/></svg>"#,
            "无法完成授权",
            "此回调链接无效或已过期。",
            "请关闭此页面，返回 Codex Relay 后重新发起授权。",
        )
    };
    let document = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{heading} · Codex Relay</title>
  {OAUTH_CALLBACK_PAGE_STYLE}
</head>
<body>
  <main>
    <section class="card" aria-labelledby="status-title">
      <div class="brand"><span class="brand-mark" aria-hidden="true">CR</span>Codex Relay</div>
      <div class="status-icon {status_class}" aria-hidden="true">{icon}</div>
      <h1 id="status-title">{heading}</h1>
      <p>{message}</p>
      <p class="next-step">{next_step}</p>
    </section>
  </main>
  <script>
    if (window.history.replaceState) {{
      window.history.replaceState(null, document.title, window.location.pathname);
    }}
  </script>
</body>
</html>"#
    );
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store, max-age=0\r\nPragma: no-cache\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{document}",
        document.len()
    )
}

async fn exchange_code(code: &str, verifier: &str) -> AppResult<CodexOAuthCredential> {
    #[derive(Deserialize)]
    struct TokenResponse {
        id_token: String,
        access_token: String,
        refresh_token: Option<String>,
    }
    let redirect = format!("http://localhost:{OAUTH_CALLBACK_PORT}/auth/callback");
    let response = reqwest::Client::new()
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect),
            ("client_id", OAUTH_CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| AppError::RuntimeUnavailable)?;
    if !response.status().is_success() {
        return Err(AppError::RuntimeUnavailable);
    }
    let token: TokenResponse = response
        .json()
        .await
        .map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(CodexOAuthCredential {
        id_token: token.id_token,
        access_token: token.access_token,
        refresh_token: token.refresh_token.filter(|value| !value.is_empty()),
        account_id: None,
        last_refresh_ms: now_ms(),
    })
}
async fn refresh_credential(credential: &CodexOAuthCredential) -> AppResult<CodexOAuthCredential> {
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
        last_refresh_ms: now_ms(),
    })
}
fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn build_authorization_url(redirect: &str, verifier: &str, state: &str) -> AppResult<String> {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = Url::parse(OAUTH_AUTHORIZE_URL).map_err(|_| AppError::Internal)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", OAUTH_SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codex_vscode");
    Ok(url.into())
}
fn credential_needs_refresh(credential: &CodexOAuthCredential) -> bool {
    let payload = credential
        .access_token
        .split('.')
        .nth(1)
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .and_then(|value| serde_json::from_slice::<serde_json::Value>(&value).ok());
    payload
        .and_then(|value| value.get("exp").and_then(|value| value.as_i64()))
        .is_some_and(|expiry| expiry * 1000 <= now_ms() + 300_000)
}
fn default_codex_home() -> AppResult<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let userprofile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let homedrive = std::env::var_os("HOMEDRIVE").map(PathBuf::from);
    let homepath = std::env::var_os("HOMEPATH").map(PathBuf::from);
    default_codex_home_from_values(
        cfg!(target_os = "windows"),
        home,
        userprofile,
        homedrive,
        homepath,
    )
    .ok_or(AppError::RuntimeUnavailable)
}
fn default_codex_home_from_values(
    windows: bool,
    home: Option<PathBuf>,
    userprofile: Option<PathBuf>,
    homedrive: Option<PathBuf>,
    homepath: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = if windows {
        userprofile.or_else(|| match (homedrive, homepath) {
            (Some(drive), Some(path)) => Some(drive.join(path)),
            _ => home,
        })
    } else {
        home
    }?;
    Some(base.join(".codex"))
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
fn oauth_status(
    attempt_id: String,
    profile_id: Option<String>,
    phase: AttemptPhase,
) -> OAuthImportStatus {
    let (phase, message) = match phase {
        AttemptPhase::Authorizing => (
            "authorizing",
            "已在默认浏览器中打开 OpenAI / ChatGPT 登录页面。",
        ),
        AttemptPhase::Authenticated => ("authenticated", "授权完成，凭据将在创建档案时保存。"),
        AttemptPhase::Failed => ("failed", "OAuth 授权或令牌交换未完成，请重试。"),
        AttemptPhase::Cancelled => ("cancelled", "OAuth 授权已取消。"),
    };
    OAuthImportStatus {
        attempt_id,
        profile_id,
        phase: phase.to_owned(),
        message: message.to_owned(),
    }
}
fn current_profile_status(
    profile_id: String,
    attempt_id: Option<String>,
    phase: CurrentProfileAttemptPhase,
    workspace_mode: DesktopWorkspaceMode,
) -> CurrentProfileActivation {
    let workspace = match workspace_mode {
        DesktopWorkspaceMode::Fresh => "全新工作区",
        DesktopWorkspaceMode::PerProfile => "档案独立工作区",
        DesktopWorkspaceMode::Shared => "共享原客户端状态",
    };
    let (status, message) = match phase {
        CurrentProfileAttemptPhase::Switching => (
            "switching",
            format!("正在写入已保存的 Codex 凭据，并启动{workspace}。"),
        ),
        CurrentProfileAttemptPhase::Activated => (
            "activated",
            format!("Codex 凭据已切换，{workspace}的 ChatGPT/Codex 桌面实例已启动。"),
        ),
        CurrentProfileAttemptPhase::AuthFileWriteFailed => (
            "auth_file_write_failed",
            "Codex 凭据未能写入默认 .codex/auth.json；请确认该目录可写后重试。".into(),
        ),
        CurrentProfileAttemptPhase::CodexKeychainWriteFailed => (
            "codex_keychain_write_failed",
            "Codex 凭据已写入，但无法更新 macOS 的 Codex Auth 钥匙串；未启动桌面实例，请解锁钥匙串后重试。".into(),
        ),
        CurrentProfileAttemptPhase::DesktopRestartFailed => (
            "desktop_restart_failed",
            format!("Codex 凭据已切换，但{workspace}的 ChatGPT/Codex 桌面实例未能启动；请确认应用已安装后重试。"),
        ),
        CurrentProfileAttemptPhase::Failed => {
            ("failed", "账号切换未完成；请检查保存的 OAuth 凭据后重试。".into())
        }
    };
    CurrentProfileActivation {
        profile_id,
        attempt_id,
        status: status.into(),
        message,
    }
}
fn desktop_switch_phase(result: AppResult<()>) -> CurrentProfileAttemptPhase {
    match result {
        Ok(()) => CurrentProfileAttemptPhase::Activated,
        Err(AppError::CodexKeychainUnavailable) => {
            CurrentProfileAttemptPhase::CodexKeychainWriteFailed
        }
        Err(_) => CurrentProfileAttemptPhase::DesktopRestartFailed,
    }
}
fn task_status(phase: ManagedTaskPhase, profile_id: Option<String>) -> ManagedTaskStatus {
    let (phase, message) = match phase {
        ManagedTaskPhase::Idle => ("idle", "当前没有正在运行的受管 Codex 任务。"),
        ManagedTaskPhase::Running => ("running", "受管 Codex 任务正在运行。"),
        ManagedTaskPhase::Completed => ("completed", "受管 Codex 任务已完成。"),
        ManagedTaskPhase::Failed => ("failed", "受管 Codex 任务未成功完成。"),
        ManagedTaskPhase::Cancelled => ("cancelled", "受管 Codex 任务已停止。"),
    };
    ManagedTaskStatus {
        phase: phase.into(),
        profile_id,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    #[test]
    fn parses_rate_limit_windows_without_exposing_credentials() {
        let quota = parse_rate_limits_response(serde_json::json!({
            "id": 2,
            "result": {
                "rateLimits": {
                    "primary": {"usedPercent": 25.5, "windowDurationMins": 300, "resetsAt": 1_735_689_600},
                    "secondary": {"usedPercent": 80, "windowDurationMins": 10_080, "resetsAt": 1_736_294_400},
                    "rateLimitReachedType": null
                }
            }
        }))
        .unwrap();

        assert_eq!(quota.status, "available");
        assert_eq!(quota.primary.as_ref().unwrap().remaining_percent, 74.5);
        assert_eq!(
            quota.primary.as_ref().unwrap().resets_at_ms,
            Some(1_735_689_600_000)
        );
        assert_eq!(
            quota.secondary.as_ref().unwrap().window_duration_mins,
            10_080
        );
        assert!(!serde_json::to_string(&quota)
            .unwrap()
            .contains("access_token"));
    }

    #[test]
    fn rejects_malformed_rate_limit_responses() {
        assert!(parse_rate_limits_response(serde_json::json!({"id": 2, "result": {}})).is_err());
    }

    #[test]
    fn parses_multi_bucket_rate_limits_and_plan_type() {
        let quota = parse_rate_limits_response(serde_json::json!({
            "id": 3,
            "result": {
                "rateLimits": {
                    "limitId": "codex",
                    "planType": "pro",
                    "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1_735_689_600},
                    "secondary": null,
                    "rateLimitReachedType": null
                },
                "rateLimitsByLimitId": {
                    "codex": {
                        "limitId": "codex",
                        "limitName": "Codex",
                        "planType": "pro",
                        "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1_735_689_600},
                        "secondary": null,
                        "rateLimitReachedType": null
                    },
                    "codex_other": {
                        "limitId": "codex_other",
                        "limitName": "Other Codex",
                        "planType": "pro",
                        "primary": null,
                        "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 1_736_294_400},
                        "rateLimitReachedType": null
                    }
                }
            }
        }))
        .unwrap();

        assert_eq!(quota.buckets.len(), 2);
        assert!(quota
            .buckets
            .iter()
            .all(|bucket| bucket.plan_type.as_deref() == Some("pro")));
        assert_eq!(
            quota
                .buckets
                .iter()
                .find(|bucket| bucket.id.as_deref() == Some("codex_other"))
                .and_then(|bucket| bucket.secondary.as_ref())
                .map(|window| window.remaining_percent),
            Some(60.0)
        );
    }

    #[test]
    fn reads_subscription_period_only_from_valid_compatible_entitlement_fields() {
        let response: AccountCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": {
                "default": {
                    "account": {"account_id": "account_123"},
                    "entitlement": {
                        "subscription_plan": "chatgptproplan",
                        "expires_at": "2026-08-01T00:00:00Z"
                    },
                    "last_active_subscription": {"will_renew": true}
                }
            }
        }))
        .unwrap();
        let subscription = account_check_snapshot(response);
        assert_eq!(subscription.account_id.as_deref(), Some("account_123"));
        assert_eq!(subscription.subscription.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            subscription.subscription.period_ends_at_ms,
            Some(1_785_542_400_000)
        );
        assert_eq!(subscription.subscription.will_renew, Some(true));

        assert!(parse_period_end_ms(&serde_json::json!("not-a-date")).is_none());
    }

    static OAUTH_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn oauth_test_lock() -> std::sync::MutexGuard<'static, ()> {
        OAUTH_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait_for_oauth_listener_release() {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", OAUTH_CALLBACK_PORT)) {
                drop(listener);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "OAuth callback listener was not released"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    struct NoopRunner;

    impl CodexRunner for NoopRunner {
        fn start_task(&self, _: &Path, _: &Path, _: &str) -> AppResult<Box<dyn RuntimeProcess>> {
            Err(AppError::RuntimeUnavailable)
        }
    }

    struct RecordingBrowser {
        urls: Mutex<Vec<String>>,
        fails: bool,
    }

    impl BrowserLauncher for RecordingBrowser {
        fn open(&self, url: &str) -> AppResult<()> {
            self.urls.lock().unwrap().push(url.to_owned());
            if self.fails {
                Err(AppError::RuntimeUnavailable)
            } else {
                Ok(())
            }
        }
    }

    fn runtime_with_browser(browser: Arc<dyn BrowserLauncher>) -> CodexRuntime {
        CodexRuntime::with_dependencies(
            PathBuf::from("/relay"),
            Arc::new(crate::secrets::MemorySecretStore::new()),
            Arc::new(NoopRunner),
            browser,
            Arc::new(SystemDesktopController),
            Arc::new(SystemDesktopCredentialStore),
        )
    }

    #[derive(Default)]
    struct RecordingDesktop {
        launches: Mutex<Vec<(PathBuf, Option<PathBuf>)>>,
        quits: Mutex<usize>,
    }

    impl DesktopController for RecordingDesktop {
        fn launch(&self, codex_home: &Path, user_data_dir: Option<&Path>) -> AppResult<()> {
            self.launches.lock().unwrap().push((
                codex_home.to_path_buf(),
                user_data_dir.map(Path::to_path_buf),
            ));
            Ok(())
        }
        fn quit_running(&self) -> AppResult<()> {
            *self.quits.lock().unwrap() += 1;
            Ok(())
        }
    }

    struct RecordingDesktopCredentialStore {
        projections: Mutex<Vec<PathBuf>>,
        fails: bool,
    }

    impl DesktopCredentialStore for RecordingDesktopCredentialStore {
        fn project(&self, codex_home: &Path, _: &str) -> AppResult<()> {
            self.projections
                .lock()
                .unwrap()
                .push(codex_home.to_path_buf());
            if self.fails {
                Err(AppError::CodexKeychainUnavailable)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn uses_cockpit_compatible_auth_file_shape() {
        let credential = CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: Some("account".into()),
            last_refresh_ms: 1,
        };
        let auth = credential.auth_json().unwrap();
        let auth_value: serde_json::Value = serde_json::from_str(&auth).unwrap();
        assert_eq!(
            CodexOAuthCredential::from_auth_json(&auth)
                .unwrap()
                .access_token,
            "access"
        );
        assert_eq!(auth_value["auth_mode"], serde_json::Value::Null);
        assert_eq!(auth_value["base_url"], serde_json::Value::Null);
        assert_eq!(auth_value["agent_identity"], serde_json::Value::Null);
        assert!(auth_value["last_refresh"].is_string());
    }
    #[test]
    fn authorization_url_has_pkce_and_cockpit_originator() {
        let url =
            build_authorization_url("http://localhost:1455/auth/callback", "verifier", "state")
                .unwrap();
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("originator=codex_vscode"));
    }

    #[test]
    fn renders_a_private_utf8_oauth_success_page() {
        let response = oauth_callback_response(true);
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();

        assert_eq!(headers.lines().next(), Some("HTTP/1.1 200 OK"));
        assert!(headers.contains("Content-Type: text/html; charset=utf-8"));
        assert!(headers.contains("Cache-Control: no-store, max-age=0"));
        assert!(headers.contains("X-Content-Type-Options: nosniff"));
        assert!(headers.contains("Content-Security-Policy:"));
        assert!(headers.contains("Content-Length: "));
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(content_length, body.len());
        assert!(body.contains("<html lang=\"zh-CN\">"));
        assert!(body.contains("<meta charset=\"utf-8\">"));
        assert!(body.contains("授权完成"));
        assert!(body.contains("安全关闭此页面"));
        assert!(body.contains("history.replaceState"));
        assert!(body.contains("window.location.pathname"));
        assert!(!body.contains("window.close"));
        for sensitive_value in ["code=secret", "state=secret", "access_token=secret"] {
            assert!(!body.contains(sensitive_value));
        }
    }

    #[test]
    fn renders_a_private_utf8_oauth_failure_page() {
        let response = oauth_callback_response(false);
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();

        assert_eq!(headers.lines().next(), Some("HTTP/1.1 400 Bad Request"));
        assert!(headers.contains("Content-Type: text/html; charset=utf-8"));
        assert!(headers.contains("Cache-Control: no-store, max-age=0"));
        assert!(headers.contains("X-Content-Type-Options: nosniff"));
        assert!(body.contains("无法完成授权"));
        assert!(body.contains("重新发起授权"));
        assert!(body.contains("history.replaceState"));
        assert!(!body.contains("code=secret"));
        assert!(!body.contains("state=secret"));
    }

    #[test]
    fn opens_the_oauth_url_in_the_default_browser() {
        let _lock = oauth_test_lock();
        let browser = Arc::new(RecordingBrowser {
            urls: Mutex::new(Vec::new()),
            fails: false,
        });
        let runtime = runtime_with_browser(browser.clone());

        let status = runtime.start_oauth_import(None).unwrap();

        let urls = browser.urls.lock().unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with(OAUTH_AUTHORIZE_URL));
        assert!(urls[0].contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455"));
        drop(urls);
        runtime.cancel_oauth_import(&status.attempt_id).unwrap();
        wait_for_oauth_listener_release();
    }

    #[test]
    fn cleans_up_the_attempt_when_the_default_browser_cannot_open() {
        let _lock = oauth_test_lock();
        let browser = Arc::new(RecordingBrowser {
            urls: Mutex::new(Vec::new()),
            fails: true,
        });
        let runtime = runtime_with_browser(browser);

        assert!(matches!(
            runtime.start_oauth_import(None),
            Err(AppError::RuntimeUnavailable)
        ));
        assert!(runtime.attempts.lock().unwrap().is_empty());
        wait_for_oauth_listener_release();
    }

    #[test]
    fn reports_switching_without_oauth_reauthorization() {
        let status = current_profile_status(
            "profile".into(),
            Some("attempt".into()),
            CurrentProfileAttemptPhase::Switching,
            DesktopWorkspaceMode::PerProfile,
        );
        assert_eq!(status.status, "switching");
        assert!(!status.message.contains("授权"));
        assert!(status.message.contains("档案独立工作区"));
    }

    #[test]
    fn reports_activation_after_a_successful_desktop_launch_request() {
        assert_eq!(
            current_profile_status(
                "profile".into(),
                Some("attempt".into()),
                desktop_switch_phase(Ok(())),
                DesktopWorkspaceMode::PerProfile,
            )
            .status,
            "activated"
        );
    }

    #[test]
    fn reports_an_auth_file_write_failure_without_attempting_a_desktop_restart() {
        assert_eq!(
            current_profile_status(
                "profile".into(),
                Some("attempt".into()),
                CurrentProfileAttemptPhase::AuthFileWriteFailed,
                DesktopWorkspaceMode::PerProfile,
            )
            .status,
            "auth_file_write_failed"
        );
    }

    #[test]
    fn projects_credentials_to_an_isolated_desktop_home_and_keychain_before_launching() {
        let root = std::env::temp_dir().join(format!("codex-relay-test-{}", Uuid::new_v4()));
        let desktop = Arc::new(RecordingDesktop::default());
        let keychain = Arc::new(RecordingDesktopCredentialStore {
            projections: Mutex::new(Vec::new()),
            fails: false,
        });
        let runtime = CodexRuntime::with_dependencies(
            root.clone(),
            Arc::new(crate::secrets::MemorySecretStore::new()),
            Arc::new(NoopRunner),
            Arc::new(RecordingBrowser {
                urls: Mutex::new(Vec::new()),
                fails: false,
            }),
            desktop.clone(),
            keychain.clone(),
        );
        let credential = CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: None,
            last_refresh_ms: 1,
        };

        runtime
            .launch_profile_desktop(
                "profile-a",
                &credential,
                &DesktopWorkspaceLaunch::PerProfile,
            )
            .unwrap();
        let home = root.join("runtimes/profile-a");
        let user_data = root.join("desktop-instances/profile-a/electron");

        assert_eq!(
            CodexOAuthCredential::from_auth_json(
                &fs::read_to_string(home.join("auth.json")).unwrap()
            )
            .unwrap()
            .access_token,
            "access"
        );
        assert_eq!(
            keychain.projections.lock().unwrap().as_slice(),
            std::slice::from_ref(&home)
        );
        assert_eq!(
            desktop.launches.lock().unwrap().as_slice(),
            &[(home, Some(user_data))]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_workspace_launch_never_receives_an_electron_data_directory() {
        let root = std::env::temp_dir().join(format!("codex-relay-test-{}", Uuid::new_v4()));
        let desktop = Arc::new(RecordingDesktop::default());
        let runtime = CodexRuntime::with_dependencies(
            root.clone(),
            Arc::new(crate::secrets::MemorySecretStore::new()),
            Arc::new(NoopRunner),
            Arc::new(RecordingBrowser {
                urls: Mutex::new(Vec::new()),
                fails: false,
            }),
            desktop.clone(),
            Arc::new(RecordingDesktopCredentialStore {
                projections: Mutex::new(Vec::new()),
                fails: false,
            }),
        );
        let credential = CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: None,
            last_refresh_ms: 1,
        };

        runtime
            .launch_profile_desktop("profile-a", &credential, &DesktopWorkspaceLaunch::Shared)
            .unwrap();

        assert_eq!(
            desktop.launches.lock().unwrap().as_slice(),
            &[(root.join("runtimes/profile-a"), None)]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_workspaces_use_distinct_recoverable_directories() {
        let runtime = runtime_with_browser(Arc::new(RecordingBrowser {
            urls: Mutex::new(Vec::new()),
            fails: false,
        }));
        assert_ne!(
            runtime.fresh_desktop_user_data("fresh-a"),
            runtime.fresh_desktop_user_data("fresh-b")
        );
        assert_eq!(
            runtime.fresh_desktop_user_data("fresh-a"),
            PathBuf::from("/relay/desktop-instances/fresh/fresh-a/electron")
        );
    }

    #[test]
    fn keychain_failure_keeps_credentials_but_never_launches_the_desktop_instance() {
        let root = std::env::temp_dir().join(format!("codex-relay-test-{}", Uuid::new_v4()));
        let desktop = Arc::new(RecordingDesktop::default());
        let runtime = CodexRuntime::with_dependencies(
            root.clone(),
            Arc::new(crate::secrets::MemorySecretStore::new()),
            Arc::new(NoopRunner),
            Arc::new(RecordingBrowser {
                urls: Mutex::new(Vec::new()),
                fails: false,
            }),
            desktop.clone(),
            Arc::new(RecordingDesktopCredentialStore {
                projections: Mutex::new(Vec::new()),
                fails: true,
            }),
        );
        let credential = CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: None,
            last_refresh_ms: 1,
        };

        assert!(matches!(
            runtime.launch_profile_desktop(
                "profile-a",
                &credential,
                &DesktopWorkspaceLaunch::PerProfile,
            ),
            Err(AppError::CodexKeychainUnavailable)
        ));
        assert!(root.join("runtimes/profile-a/auth.json").exists());
        assert!(desktop.launches.lock().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derives_stable_distinct_keychain_accounts_for_each_codex_home() {
        assert_eq!(
            codex_keychain_account(Path::new("/relay/runtimes/a")),
            codex_keychain_account(Path::new("/relay/runtimes/a"))
        );
        assert_ne!(
            codex_keychain_account(Path::new("/relay/runtimes/a")),
            codex_keychain_account(Path::new("/relay/runtimes/b"))
        );
    }

    #[test]
    fn resolves_default_codex_home_for_macos_and_windows() {
        assert_eq!(DESKTOP_APP_CANDIDATES, ["ChatGPT", "Codex"]);
        assert_eq!(
            default_codex_home_from_values(
                false,
                Some(PathBuf::from("/Users/example")),
                None,
                None,
                None,
            ),
            Some(PathBuf::from("/Users/example/.codex"))
        );
        assert_eq!(
            default_codex_home_from_values(
                true,
                None,
                Some(PathBuf::from(r"C:\\Users\\example")),
                None,
                None,
            ),
            Some(PathBuf::from(r"C:\\Users\\example/.codex"))
        );
    }

    #[test]
    fn reports_desktop_restart_failure_without_rolling_back_codex_projection() {
        assert_eq!(
            desktop_switch_phase(Err(AppError::DesktopUnavailable)),
            CurrentProfileAttemptPhase::DesktopRestartFailed
        );
    }

    #[test]
    fn reports_keychain_failure_without_launching_a_login_prompt() {
        assert_eq!(
            desktop_switch_phase(Err(AppError::CodexKeychainUnavailable)),
            CurrentProfileAttemptPhase::CodexKeychainWriteFailed
        );
        assert_eq!(
            current_profile_status(
                "profile".into(),
                Some("attempt".into()),
                CurrentProfileAttemptPhase::CodexKeychainWriteFailed,
                DesktopWorkspaceMode::PerProfile,
            )
            .status,
            "codex_keychain_write_failed"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bounds_desktop_launch_commands_and_reports_failures() {
        let mut succeeds = Command::new("true");
        assert!(run_command_with_timeout(&mut succeeds, Duration::from_secs(1)).is_ok());

        let mut fails = Command::new("false");
        assert!(matches!(
            run_command_with_timeout(&mut fails, Duration::from_secs(1)),
            Err(AppError::DesktopUnavailable)
        ));

        let mut blocks = Command::new("sh");
        blocks.args(["-c", "while :; do :; done"]);
        assert!(matches!(
            run_command_with_timeout(&mut blocks, Duration::ZERO),
            Err(AppError::DesktopUnavailable)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn builds_an_isolated_launchservices_request_for_the_selected_profile() {
        let args = macos_desktop_launch_args(
            "ChatGPT",
            Path::new("/relay/runtimes/profile-a"),
            Some(Path::new("/relay/desktop-instances/profile-a/electron")),
        );

        assert_eq!(args[0..3], ["-n", "-a", "ChatGPT"]);
        assert!(args.contains(&"CODEX_HOME=/relay/runtimes/profile-a".into()));
        assert!(args.contains(
            &"CODEX_ELECTRON_USER_DATA_PATH=/relay/desktop-instances/profile-a/electron".into()
        ));
        assert!(
            args.contains(&"--user-data-dir=/relay/desktop-instances/profile-a/electron".into())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shared_launchservices_request_omits_electron_user_data_arguments() {
        let args =
            macos_desktop_launch_args("ChatGPT", Path::new("/relay/runtimes/profile-a"), None);
        assert!(args.contains(&"CODEX_HOME=/relay/runtimes/profile-a".into()));
        assert!(!args.iter().any(|argument| argument.contains("USER_DATA")));
        assert!(!args
            .iter()
            .any(|argument| argument.contains("user-data-dir")));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn builds_a_windows_launch_request_with_isolated_environment() {
        let command = windows_desktop_command(
            Path::new(r"C:\\Apps\\ChatGPT.exe"),
            Path::new(r"C:\\Relay\\runtimes\\profile-a"),
            Some(Path::new(
                r"C:\\Relay\\desktop-instances\\profile-a\\electron",
            )),
        );

        assert_eq!(command.get_program(), Path::new(r"C:\\Apps\\ChatGPT.exe"));
        assert!(command
            .get_args()
            .any(|arg| arg == "--user-data-dir=C:\\Relay\\desktop-instances\\profile-a\\electron"));
        let environment = command.get_envs().collect::<HashMap<_, _>>();
        assert_eq!(
            environment.get(std::ffi::OsStr::new("CODEX_HOME")),
            Some(&Some(std::ffi::OsStr::new(
                r"C:\\Relay\\runtimes\\profile-a"
            )))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("CODEX_ELECTRON_USER_DATA_PATH")),
            Some(&Some(std::ffi::OsStr::new(
                r"C:\\Relay\\desktop-instances\\profile-a\\electron"
            )))
        );
    }
}
