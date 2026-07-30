use std::{
    collections::HashSet,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::collections::HashMap;

use crate::{
    domain::{
        CodexEnvironmentCheck, CodexEnvironmentInstallLog, CodexEnvironmentInstallReport,
        CodexEnvironmentInstallStep, CodexEnvironmentReport, CodexEnvironmentSummary,
        InstallCodexEnvironmentInput,
    },
    error::{AppError, AppResult},
};

const OAUTH_CALLBACK_PORT: u16 = 1455;
const STEP_NODE: &str = "node";
const STEP_NPM: &str = "npm";
const STEP_GIT: &str = "git";
const STEP_CODEX_CLI: &str = "codex_cli";
const STEP_CODEX_HOME: &str = "codex_home";
const STEP_BROWSER: &str = "browser";
const STEP_RELAY_CA: &str = "relay_ca";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvironmentPlatform {
    Windows,
    Macos,
    Linux,
    Other,
}

impl EnvironmentPlatform {
    fn current() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::Macos,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
    display: String,
    requires_privilege: bool,
    next_action: Option<String>,
}

impl CommandSpec {
    fn new(program: &str, args: &[&str], display: &str) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            display: display.to_owned(),
            requires_privilege: false,
            next_action: None,
        }
    }

    fn privileged(mut self) -> Self {
        self.requires_privilege = true;
        self
    }

    fn with_next_action(mut self, next_action: impl Into<String>) -> Self {
        self.next_action = Some(next_action.into());
        self
    }
}

#[derive(Debug, Clone)]
struct CommandResult {
    success: bool,
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

trait CommandExecutor {
    fn output(&self, command: &CommandSpec) -> Result<CommandResult, String>;
}

type CheckValidator = fn(&str) -> Option<(String, String)>;

struct CommandCheckSpec<'a> {
    id: &'a str,
    label: &'a str,
    command: &'a str,
    args: &'a [&'a str],
    install_command: Option<String>,
    description: Option<&'a str>,
    validator: Option<CheckValidator>,
}

struct SystemCommandExecutor;

impl CommandExecutor for SystemCommandExecutor {
    fn output(&self, command: &CommandSpec) -> Result<CommandResult, String> {
        let output = Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code(),
        })
    }
}

#[derive(Debug, Clone, Default)]
struct ToolInventory {
    winget: bool,
    brew: bool,
    pkexec: bool,
    apt_get: bool,
    dnf: bool,
    pacman: bool,
    zypper: bool,
    npm: bool,
    security: bool,
    certutil: bool,
    openssl: bool,
    browser: bool,
}

pub(crate) fn default_codex_home() -> AppResult<PathBuf> {
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

pub(crate) fn default_codex_home_from_values(
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

pub(crate) fn status(ca_path: Option<PathBuf>) -> AppResult<CodexEnvironmentReport> {
    environment_report(
        &SystemCommandExecutor,
        EnvironmentPlatform::current(),
        ca_path,
    )
}

pub(crate) fn install(
    input: InstallCodexEnvironmentInput,
    ca_path: Option<PathBuf>,
) -> AppResult<CodexEnvironmentInstallReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    install_with_executor(
        input,
        &SystemCommandExecutor,
        EnvironmentPlatform::current(),
        ca_path,
    )
}

fn environment_report(
    executor: &dyn CommandExecutor,
    platform: EnvironmentPlatform,
    ca_path: Option<PathBuf>,
) -> AppResult<CodexEnvironmentReport> {
    let inventory = inventory(executor, platform);
    let codex_home = default_codex_home().ok();
    let checks = environment_checks(executor, platform, &inventory, codex_home.as_ref(), ca_path);
    let install_steps = install_steps(platform, codex_home.as_ref(), &inventory);
    let summary = environment_summary(&checks, &install_steps);
    let manual_commands = checks
        .iter()
        .filter(|check| check.status != "ok")
        .filter_map(|check| check.command.clone())
        .collect::<Vec<_>>();
    let message = environment_message(platform, &summary, &install_steps);
    Ok(CodexEnvironmentReport {
        platform: platform.as_str().to_owned(),
        codex_home: codex_home.map(|path| path.display().to_string()),
        can_install: summary.fixable_count > 0,
        message,
        last_checked_at_ms: timestamp_ms(),
        summary,
        checks,
        install_steps,
        manual_commands,
    })
}

fn install_with_executor(
    input: InstallCodexEnvironmentInput,
    executor: &dyn CommandExecutor,
    platform: EnvironmentPlatform,
    ca_path: Option<PathBuf>,
) -> AppResult<CodexEnvironmentInstallReport> {
    let before = environment_report(executor, platform, ca_path.clone())?;
    let selected = selected_install_steps(&input, &before);
    let inventory = inventory(executor, platform);
    let codex_home = default_codex_home().ok();
    let steps = install_steps(platform, codex_home.as_ref(), &inventory);
    let mut logs = Vec::new();
    let mut executed_displays = HashSet::new();
    for step in steps {
        if !selected.contains(&step.id) {
            continue;
        }
        if let Some(command) = &step.command {
            if !executed_displays.insert(command.clone()) {
                logs.push(CodexEnvironmentInstallLog {
                    step_id: step.id.clone(),
                    label: step.label.clone(),
                    status: "skipped".to_owned(),
                    detail: "该安装命令已由同组步骤执行，跳过重复运行。".to_owned(),
                    command: step.command.clone(),
                    next_action: Some("安装完成后重新检查环境状态。".to_owned()),
                });
                continue;
            }
        }
        logs.push(run_install_step(executor, platform, &inventory, &step));
    }
    let environment = environment_report(executor, platform, ca_path)?;
    let status = if logs.iter().any(|log| {
        matches!(
            log.status.as_str(),
            "failed" | "needs_privilege" | "unsupported"
        )
    }) {
        "failed"
    } else {
        "completed"
    };
    let message = if logs.is_empty() {
        "没有检测到可自动部署的缺失项；请按检查卡片中的命令手动处理。"
    } else if status == "completed" {
        "Codex 环境部署步骤已执行。请重新打开终端或重启 Codex Relay 以刷新 PATH。"
    } else {
        "部分 Codex 环境部署步骤需要继续处理；请展开日志查看原因和下一步。"
    };
    Ok(CodexEnvironmentInstallReport {
        status: status.to_owned(),
        message: message.to_owned(),
        logs,
        environment,
    })
}

fn selected_install_steps(
    input: &InstallCodexEnvironmentInput,
    report: &CodexEnvironmentReport,
) -> HashSet<String> {
    if let Some(steps) = input.steps.as_ref().filter(|steps| !steps.is_empty()) {
        return steps.iter().cloned().collect();
    }
    let available = report
        .install_steps
        .iter()
        .filter(|step| step.available)
        .map(|step| step.id.clone())
        .collect::<HashSet<_>>();
    report
        .checks
        .iter()
        .filter(|check| check.status == "missing" && available.contains(&check.id))
        .map(|check| check.id.clone())
        .collect()
}

fn environment_checks(
    executor: &dyn CommandExecutor,
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
    codex_home: Option<&PathBuf>,
    ca_path: Option<PathBuf>,
) -> Vec<CodexEnvironmentCheck> {
    let mut checks = vec![
        command_check(
            executor,
            CommandCheckSpec {
                id: STEP_NODE,
                label: "Node.js LTS",
                command: "node",
                args: &["--version"],
                install_command: install_command_display(platform, inventory, STEP_NODE),
                description: Some("Codex CLI 的 npm 安装和本地工具链依赖 Node.js。"),
                validator: Some(validate_node_version),
            },
        ),
        command_check(
            executor,
            CommandCheckSpec {
                id: STEP_NPM,
                label: "npm",
                command: "npm",
                args: &["--version"],
                install_command: install_command_display(platform, inventory, STEP_NPM),
                description: Some("用于安装和更新 @openai/codex。"),
                validator: None,
            },
        ),
        command_check(
            executor,
            CommandCheckSpec {
                id: STEP_CODEX_CLI,
                label: "Codex CLI",
                command: "codex",
                args: &["--version"],
                install_command: install_command_display(platform, inventory, STEP_CODEX_CLI),
                description: Some("协作任务、模型刷新和部分本机 Codex 能力需要 CLI 可执行文件。"),
                validator: None,
            },
        ),
        command_check(
            executor,
            CommandCheckSpec {
                id: STEP_GIT,
                label: "Git",
                command: "git",
                args: &["--version"],
                install_command: install_command_display(platform, inventory, STEP_GIT),
                description: Some("Codex 任务和项目协作依赖 Git 读取仓库状态。"),
                validator: None,
            },
        ),
        codex_home_check(codex_home, platform),
        oauth_port_check(platform),
        browser_check(platform, inventory),
        relay_ca_check(executor, platform, inventory, ca_path),
    ];
    checks.sort_by_key(|check| match check.id.as_str() {
        STEP_NODE => 0,
        STEP_NPM => 1,
        STEP_CODEX_CLI => 2,
        STEP_GIT => 3,
        STEP_CODEX_HOME => 4,
        "oauth_callback_port" => 5,
        STEP_BROWSER => 6,
        STEP_RELAY_CA => 7,
        _ => 8,
    });
    checks
}

fn environment_summary(
    checks: &[CodexEnvironmentCheck],
    install_steps: &[CodexEnvironmentInstallStep],
) -> CodexEnvironmentSummary {
    let available = install_steps
        .iter()
        .filter(|step| step.available)
        .map(|step| step.id.as_str())
        .collect::<HashSet<_>>();
    let ok_count = checks.iter().filter(|check| check.status == "ok").count();
    let warning_count = checks
        .iter()
        .filter(|check| check.status == "warning")
        .count();
    let missing_count = checks
        .iter()
        .filter(|check| check.status == "missing")
        .count();
    let failed_count = checks
        .iter()
        .filter(|check| check.status == "failed")
        .count();
    let fixable_count = checks
        .iter()
        .filter(|check| check.status == "missing" && available.contains(check.id.as_str()))
        .count();
    let score = if checks.is_empty() {
        100
    } else {
        ((ok_count * 100 + warning_count * 60) / checks.len()).min(100) as u8
    };
    let status = if failed_count > 0 || missing_count > 0 {
        "action_required"
    } else if warning_count > 0 {
        "warning"
    } else {
        "healthy"
    };
    CodexEnvironmentSummary {
        status: status.to_owned(),
        ok_count,
        warning_count,
        missing_count,
        failed_count,
        fixable_count,
        health_percent: score,
    }
}

fn environment_message(
    platform: EnvironmentPlatform,
    summary: &CodexEnvironmentSummary,
    install_steps: &[CodexEnvironmentInstallStep],
) -> String {
    if summary.status == "healthy" {
        return "Codex 三端最小运行环境检查通过。".to_owned();
    }
    let platform_name = platform_display(platform);
    if summary.fixable_count > 0 {
        return format!(
            "{platform_name} 检测到 {} 个可自动处理的缺失项，可执行一键部署。",
            summary.fixable_count
        );
    }
    if install_steps.iter().all(|step| !step.available) && summary.missing_count > 0 {
        return format!(
            "{platform_name} 检测到缺失项，但当前缺少可用包管理器或授权代理；请按命令手动处理。"
        );
    }
    format!(
        "{platform_name} 检测到 {} 个提示项，请查看检查卡片中的原因和下一步。",
        summary.warning_count + summary.missing_count + summary.failed_count
    )
}

fn command_check(
    executor: &dyn CommandExecutor,
    spec: CommandCheckSpec<'_>,
) -> CodexEnvironmentCheck {
    let command = CommandSpec::new(
        spec.command,
        spec.args,
        &format!("{} {}", spec.command, spec.args.join(" ")),
    );
    match executor.output(&command) {
        Ok(output) if output.success => {
            let detail = concise_command_output(&output.stdout, &output.stderr);
            if let Some((status, next_action)) =
                spec.validator.and_then(|validate| validate(&detail))
            {
                return CodexEnvironmentCheck {
                    id: spec.id.to_owned(),
                    label: spec.label.to_owned(),
                    status,
                    detail,
                    command: spec.install_command,
                    description: spec.description.map(str::to_owned),
                    next_action: Some(next_action),
                    automatic: false,
                };
            }
            CodexEnvironmentCheck {
                id: spec.id.to_owned(),
                label: spec.label.to_owned(),
                status: "ok".to_owned(),
                detail,
                command: None,
                description: spec.description.map(str::to_owned),
                next_action: Some("无需处理。".to_owned()),
                automatic: false,
            }
        }
        Ok(output) => CodexEnvironmentCheck {
            id: spec.id.to_owned(),
            label: spec.label.to_owned(),
            status: "missing".to_owned(),
            detail: format!(
                "未在 PATH 中检测到 {}（退出码：{}）。",
                spec.command,
                output
                    .code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
            command: spec.install_command.clone(),
            description: spec.description.map(str::to_owned),
            next_action: Some(next_action_for_missing(
                spec.command,
                spec.install_command.as_deref(),
            )),
            automatic: spec.install_command.is_some(),
        },
        Err(error) => CodexEnvironmentCheck {
            id: spec.id.to_owned(),
            label: spec.label.to_owned(),
            status: "missing".to_owned(),
            detail: format!("未在 PATH 中检测到 {}：{error}", spec.command),
            command: spec.install_command.clone(),
            description: spec.description.map(str::to_owned),
            next_action: Some(next_action_for_missing(
                spec.command,
                spec.install_command.as_deref(),
            )),
            automatic: spec.install_command.is_some(),
        },
    }
}

fn validate_node_version(detail: &str) -> Option<(String, String)> {
    let major = detail
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|part| part.parse::<u64>().ok())?;
    (major < 20).then(|| {
        (
            "warning".to_owned(),
            "建议升级到当前 Node.js LTS（至少 20.x；推荐 24.x），避免 Codex CLI 安装失败。"
                .to_owned(),
        )
    })
}

fn next_action_for_missing(command: &str, install_command: Option<&str>) -> String {
    install_command.map_or_else(
        || format!("请安装 {command} 并确保它出现在 PATH 中。"),
        |install| format!("可执行修复命令：{install}"),
    )
}

fn codex_home_check(
    codex_home: Option<&PathBuf>,
    platform: EnvironmentPlatform,
) -> CodexEnvironmentCheck {
    let Some(path) = codex_home else {
        return CodexEnvironmentCheck {
            id: STEP_CODEX_HOME.to_owned(),
            label: "Codex home".to_owned(),
            status: "missing".to_owned(),
            detail: "默认 Codex home 目录解析失败。".to_owned(),
            command: Some(codex_home_command(platform, None)),
            description: Some("保存 Codex config.toml、auth.json 和模型目录。".to_owned()),
            next_action: Some("请确认 HOME/USERPROFILE 环境变量存在并可读。".to_owned()),
            automatic: true,
        };
    };
    let command = codex_home_command(platform, Some(path));
    if !path.exists() {
        return CodexEnvironmentCheck {
            id: STEP_CODEX_HOME.to_owned(),
            label: "Codex home".to_owned(),
            status: "missing".to_owned(),
            detail: format!("{} 尚未创建。", path.display()),
            command: Some(command),
            description: Some("保存 Codex config.toml、auth.json 和模型目录。".to_owned()),
            next_action: Some("创建目录，不覆盖已有 config.toml。".to_owned()),
            automatic: true,
        };
    }
    if !path.is_dir() {
        return CodexEnvironmentCheck {
            id: STEP_CODEX_HOME.to_owned(),
            label: "Codex home".to_owned(),
            status: "failed".to_owned(),
            detail: format!("{} 存在但不是目录。", path.display()),
            command: None,
            description: Some("保存 Codex config.toml、auth.json 和模型目录。".to_owned()),
            next_action: Some("请移除或重命名该文件后重新检查。".to_owned()),
            automatic: false,
        };
    }
    let writable_probe = path.join(".codex-relay-write-test");
    match fs::write(&writable_probe, b"ok").and_then(|()| fs::remove_file(&writable_probe)) {
        Ok(()) => CodexEnvironmentCheck {
            id: STEP_CODEX_HOME.to_owned(),
            label: "Codex home".to_owned(),
            status: "ok".to_owned(),
            detail: path.display().to_string(),
            command: None,
            description: Some("保存 Codex config.toml、auth.json 和模型目录。".to_owned()),
            next_action: Some("目录存在且可写。".to_owned()),
            automatic: false,
        },
        Err(error) => CodexEnvironmentCheck {
            id: STEP_CODEX_HOME.to_owned(),
            label: "Codex home".to_owned(),
            status: "failed".to_owned(),
            detail: format!("{} 不可写：{error}", path.display()),
            command: None,
            description: Some("保存 Codex config.toml、auth.json 和模型目录。".to_owned()),
            next_action: Some("请修复目录权限后重新检查。".to_owned()),
            automatic: false,
        },
    }
}

fn oauth_port_check(platform: EnvironmentPlatform) -> CodexEnvironmentCheck {
    match TcpListener::bind(("127.0.0.1", OAUTH_CALLBACK_PORT)) {
        Ok(listener) => {
            drop(listener);
            CodexEnvironmentCheck {
                id: "oauth_callback_port".to_owned(),
                label: "OAuth 回调端口".to_owned(),
                status: "ok".to_owned(),
                detail: format!("127.0.0.1:{OAUTH_CALLBACK_PORT} 可用。"),
                command: None,
                description: Some("官方 OAuth 登录会在本机端口接收回调。".to_owned()),
                next_action: Some("无需处理。".to_owned()),
                automatic: false,
            }
        }
        Err(error) => CodexEnvironmentCheck {
            id: "oauth_callback_port".to_owned(),
            label: "OAuth 回调端口".to_owned(),
            status: "warning".to_owned(),
            detail: format!("127.0.0.1:{OAUTH_CALLBACK_PORT} 当前不可绑定：{error}"),
            command: Some(port_diagnostic_command(platform)),
            description: Some("官方 OAuth 登录会在本机端口接收回调。".to_owned()),
            next_action: Some("关闭占用该端口的进程后再启动官方登录。".to_owned()),
            automatic: false,
        },
    }
}

fn browser_check(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
) -> CodexEnvironmentCheck {
    if inventory.browser {
        CodexEnvironmentCheck {
            id: STEP_BROWSER.to_owned(),
            label: "默认浏览器".to_owned(),
            status: "ok".to_owned(),
            detail: browser_detail(platform),
            command: None,
            description: Some("用于打开官方 OAuth 授权页面。".to_owned()),
            next_action: Some("无需处理。".to_owned()),
            automatic: false,
        }
    } else {
        CodexEnvironmentCheck {
            id: STEP_BROWSER.to_owned(),
            label: "默认浏览器".to_owned(),
            status: "warning".to_owned(),
            detail: "未检测到可用的系统浏览器打开命令。".to_owned(),
            command: Some(browser_fix_command(platform)),
            description: Some("用于打开官方 OAuth 授权页面。".to_owned()),
            next_action: Some("设置默认浏览器，或安装 xdg-utils/gio 后重试。".to_owned()),
            automatic: false,
        }
    }
}

fn relay_ca_check(
    executor: &dyn CommandExecutor,
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
    ca_path: Option<PathBuf>,
) -> CodexEnvironmentCheck {
    let Some(path) = ca_path else {
        return CodexEnvironmentCheck {
            id: STEP_RELAY_CA.to_owned(),
            label: "Relay CA 信任".to_owned(),
            status: "warning".to_owned(),
            detail: "网关 CA 尚未生成；启动网关后可检查信任状态。".to_owned(),
            command: None,
            description: Some(
                "第三方 provider 切换到 Codex 时需要本机信任 Relay HTTPS CA。".to_owned(),
            ),
            next_action: Some("启动网关或切换第三方 provider 时会自动生成证书。".to_owned()),
            automatic: false,
        };
    };
    if !path.exists() {
        return CodexEnvironmentCheck {
            id: STEP_RELAY_CA.to_owned(),
            label: "Relay CA 信任".to_owned(),
            status: "warning".to_owned(),
            detail: format!("{} 尚未生成。", path.display()),
            command: None,
            description: Some(
                "第三方 provider 切换到 Codex 时需要本机信任 Relay HTTPS CA。".to_owned(),
            ),
            next_action: Some("启动网关或切换第三方 provider 时会自动生成证书。".to_owned()),
            automatic: false,
        };
    }
    let verify_command = ca_verify_command(platform, inventory, &path);
    let Some(command) = verify_command else {
        return CodexEnvironmentCheck {
            id: STEP_RELAY_CA.to_owned(),
            label: "Relay CA 信任".to_owned(),
            status: "warning".to_owned(),
            detail: "当前平台缺少可用的 CA 信任检测工具。".to_owned(),
            command: ca_trust_manual_command(platform, &path),
            description: Some(
                "第三方 provider 切换到 Codex 时需要本机信任 Relay HTTPS CA。".to_owned(),
            ),
            next_action: Some("可在网关页执行信任 CA，或按命令手动导入。".to_owned()),
            automatic: false,
        };
    };
    match executor.output(&command) {
        Ok(output) if output.success => CodexEnvironmentCheck {
            id: STEP_RELAY_CA.to_owned(),
            label: "Relay CA 信任".to_owned(),
            status: "ok".to_owned(),
            detail: "Relay CA 已通过系统信任校验。".to_owned(),
            command: None,
            description: Some(
                "第三方 provider 切换到 Codex 时需要本机信任 Relay HTTPS CA。".to_owned(),
            ),
            next_action: Some("无需处理。".to_owned()),
            automatic: false,
        },
        Ok(output) => CodexEnvironmentCheck {
            id: STEP_RELAY_CA.to_owned(),
            label: "Relay CA 信任".to_owned(),
            status: "warning".to_owned(),
            detail: format!(
                "Relay CA 尚未通过系统信任校验：{}",
                concise_command_output(&output.stdout, &output.stderr)
            ),
            command: ca_trust_manual_command(platform, &path),
            description: Some(
                "第三方 provider 切换到 Codex 时需要本机信任 Relay HTTPS CA。".to_owned(),
            ),
            next_action: Some("可在网关页执行信任 CA，或按命令手动导入。".to_owned()),
            automatic: false,
        },
        Err(error) => CodexEnvironmentCheck {
            id: STEP_RELAY_CA.to_owned(),
            label: "Relay CA 信任".to_owned(),
            status: "warning".to_owned(),
            detail: format!("Relay CA 信任状态检测未完成：{error}"),
            command: ca_trust_manual_command(platform, &path),
            description: Some(
                "第三方 provider 切换到 Codex 时需要本机信任 Relay HTTPS CA。".to_owned(),
            ),
            next_action: Some("可在网关页执行信任 CA，或按命令手动导入。".to_owned()),
            automatic: false,
        },
    }
}

fn inventory(executor: &dyn CommandExecutor, platform: EnvironmentPlatform) -> ToolInventory {
    ToolInventory {
        winget: command_available(executor, platform, "winget"),
        brew: command_available(executor, platform, "brew"),
        pkexec: command_available(executor, platform, "pkexec"),
        apt_get: command_available(executor, platform, "apt-get"),
        dnf: command_available(executor, platform, "dnf"),
        pacman: command_available(executor, platform, "pacman"),
        zypper: command_available(executor, platform, "zypper"),
        npm: command_available(executor, platform, "npm"),
        security: command_available(executor, platform, "security"),
        certutil: command_available(executor, platform, "certutil.exe")
            || command_available(executor, platform, "certutil"),
        openssl: command_available(executor, platform, "openssl"),
        browser: browser_available(executor, platform),
    }
}

fn command_available(
    executor: &dyn CommandExecutor,
    platform: EnvironmentPlatform,
    command: &str,
) -> bool {
    let spec = match platform {
        EnvironmentPlatform::Windows => {
            CommandSpec::new("where.exe", &[command], &format!("where.exe {command}"))
        }
        _ => CommandSpec::new(
            "sh",
            &["-lc", &format!("command -v {}", shell_word(command))],
            &format!("command -v {command}"),
        ),
    };
    executor.output(&spec).is_ok_and(|output| output.success)
}

fn browser_available(executor: &dyn CommandExecutor, platform: EnvironmentPlatform) -> bool {
    match platform {
        EnvironmentPlatform::Windows => {
            command_available(executor, platform, "rundll32.exe")
                || command_available(executor, platform, "explorer.exe")
        }
        EnvironmentPlatform::Macos => command_available(executor, platform, "open"),
        EnvironmentPlatform::Linux => ["xdg-open", "gio", "sensible-browser"]
            .iter()
            .any(|command| command_available(executor, platform, command)),
        EnvironmentPlatform::Other => false,
    }
}

fn install_steps(
    platform: EnvironmentPlatform,
    codex_home: Option<&PathBuf>,
    inventory: &ToolInventory,
) -> Vec<CodexEnvironmentInstallStep> {
    [
        STEP_NODE,
        STEP_NPM,
        STEP_GIT,
        STEP_CODEX_CLI,
        STEP_CODEX_HOME,
    ]
    .iter()
    .map(|id| install_step(platform, codex_home, inventory, id))
    .collect()
}

fn install_step(
    platform: EnvironmentPlatform,
    codex_home: Option<&PathBuf>,
    inventory: &ToolInventory,
    id: &str,
) -> CodexEnvironmentInstallStep {
    let command = install_command(platform, codex_home, inventory, id);
    let label = match id {
        STEP_NODE => "安装 Node.js LTS",
        STEP_NPM => "安装 npm",
        STEP_GIT => "安装 Git",
        STEP_CODEX_CLI => "安装 Codex CLI",
        STEP_CODEX_HOME => "创建 Codex home",
        _ => "修复环境",
    };
    CodexEnvironmentInstallStep {
        id: id.to_owned(),
        label: label.to_owned(),
        available: command.is_some(),
        command: command.as_ref().map(|command| command.display.clone()),
        requires_privilege: command
            .as_ref()
            .is_some_and(|command| command.requires_privilege),
        next_action: command
            .as_ref()
            .and_then(|command| command.next_action.clone()),
    }
}

fn install_command(
    platform: EnvironmentPlatform,
    codex_home: Option<&PathBuf>,
    inventory: &ToolInventory,
    id: &str,
) -> Option<CommandSpec> {
    match id {
        STEP_NODE | STEP_NPM => install_node_command(platform, inventory),
        STEP_GIT => install_git_command(platform, inventory),
        STEP_CODEX_CLI => install_codex_cli_command(platform, inventory),
        STEP_CODEX_HOME => codex_home.map(|path| {
            #[cfg(target_os = "windows")]
            let _ = platform;
            let display = codex_home_command(platform, Some(path));
            if platform == EnvironmentPlatform::Windows {
                CommandSpec::new(
                    "cmd.exe",
                    &["/C", "mkdir", &path.display().to_string()],
                    &display,
                )
            } else {
                CommandSpec::new("mkdir", &["-p", &path.display().to_string()], &display)
            }
            .with_next_action("创建目录，不覆盖已有 config.toml。")
        }),
        _ => None,
    }
}

fn install_node_command(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
) -> Option<CommandSpec> {
    match platform {
        EnvironmentPlatform::Windows if inventory.winget => Some(CommandSpec::new(
            "winget",
            &["install", "--id", "OpenJS.NodeJS.LTS", "-e"],
            "winget install --id OpenJS.NodeJS.LTS -e",
        )),
        EnvironmentPlatform::Macos if inventory.brew => Some(CommandSpec::new(
            "brew",
            &["install", "node"],
            "brew install node",
        )),
        EnvironmentPlatform::Linux => linux_package_command(inventory, &["nodejs", "npm"]),
        _ => None,
    }
}

fn install_git_command(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
) -> Option<CommandSpec> {
    match platform {
        EnvironmentPlatform::Windows if inventory.winget => Some(CommandSpec::new(
            "winget",
            &["install", "--id", "Git.Git", "-e"],
            "winget install --id Git.Git -e",
        )),
        EnvironmentPlatform::Macos if inventory.brew => Some(CommandSpec::new(
            "brew",
            &["install", "git"],
            "brew install git",
        )),
        EnvironmentPlatform::Macos => Some(CommandSpec::new(
            "xcode-select",
            &["--install"],
            "xcode-select --install",
        )),
        EnvironmentPlatform::Linux => linux_package_command(inventory, &["git"]),
        _ => None,
    }
}

fn install_codex_cli_command(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
) -> Option<CommandSpec> {
    match platform {
        EnvironmentPlatform::Windows => Some(CommandSpec::new(
            "cmd.exe",
            &["/C", "npm.cmd", "install", "--global", "@openai/codex"],
            "npm.cmd install --global @openai/codex",
        )),
        EnvironmentPlatform::Macos | EnvironmentPlatform::Linux if inventory.npm => Some(
            CommandSpec::new(
                "npm",
                &["install", "--global", "@openai/codex"],
                "npm install --global @openai/codex",
            )
            .with_next_action("如果全局 npm 目录需要权限，请按日志中的 sudo 命令重试。"),
        ),
        _ => None,
    }
}

fn linux_package_command(inventory: &ToolInventory, packages: &[&str]) -> Option<CommandSpec> {
    let package_list = packages.join(" ");
    let command = if inventory.apt_get {
        format!("apt-get update && apt-get install -y {package_list}")
    } else if inventory.dnf {
        format!("dnf install -y {package_list}")
    } else if inventory.pacman {
        format!("pacman -Sy --noconfirm {package_list}")
    } else if inventory.zypper {
        format!("zypper --non-interactive install {package_list}")
    } else {
        return None;
    };
    if inventory.pkexec {
        Some(
            CommandSpec::new(
                "pkexec",
                &["sh", "-lc", &command],
                &format!("pkexec sh -lc '{}'", command.replace('\'', "'\\''")),
            )
            .privileged()
            .with_next_action("按系统授权弹窗完成安装。"),
        )
    } else {
        None
    }
}

fn install_command_display(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
    id: &str,
) -> Option<String> {
    install_command(platform, default_codex_home().ok().as_ref(), inventory, id)
        .map(|command| command.display)
        .or_else(|| manual_install_command(platform, inventory, id))
}

fn manual_install_command(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
    id: &str,
) -> Option<String> {
    match (platform, id) {
        (EnvironmentPlatform::Windows, STEP_NODE | STEP_NPM) => {
            Some("winget install --id OpenJS.NodeJS.LTS -e".to_owned())
        }
        (EnvironmentPlatform::Windows, STEP_GIT) => {
            Some("winget install --id Git.Git -e".to_owned())
        }
        (EnvironmentPlatform::Windows, STEP_CODEX_CLI) => {
            Some("npm.cmd install --global @openai/codex".to_owned())
        }
        (EnvironmentPlatform::Macos, STEP_NODE | STEP_NPM) => Some("brew install node".to_owned()),
        (EnvironmentPlatform::Macos, STEP_GIT) if inventory.brew => {
            Some("brew install git".to_owned())
        }
        (EnvironmentPlatform::Macos, STEP_GIT) => Some("xcode-select --install".to_owned()),
        (EnvironmentPlatform::Macos, STEP_CODEX_CLI) => {
            Some("npm install --global @openai/codex".to_owned())
        }
        (EnvironmentPlatform::Linux, STEP_NODE | STEP_NPM) => {
            linux_manual_package_command(inventory, &["nodejs", "npm"])
        }
        (EnvironmentPlatform::Linux, STEP_GIT) => linux_manual_package_command(inventory, &["git"]),
        (EnvironmentPlatform::Linux, STEP_CODEX_CLI) => {
            Some("npm install --global @openai/codex".to_owned())
        }
        (_, STEP_CODEX_HOME) => default_codex_home()
            .ok()
            .map(|path| codex_home_command(platform, Some(&path))),
        _ => None,
    }
}

fn linux_manual_package_command(inventory: &ToolInventory, packages: &[&str]) -> Option<String> {
    let package_list = packages.join(" ");
    if inventory.apt_get {
        Some(format!(
            "sudo sh -lc 'apt-get update && apt-get install -y {package_list}'"
        ))
    } else if inventory.dnf {
        Some(format!("sudo dnf install -y {package_list}"))
    } else if inventory.pacman {
        Some(format!("sudo pacman -Sy --noconfirm {package_list}"))
    } else if inventory.zypper {
        Some(format!(
            "sudo zypper --non-interactive install {package_list}"
        ))
    } else {
        Some(format!("请使用系统包管理器安装：{package_list}"))
    }
}

fn run_install_step(
    executor: &dyn CommandExecutor,
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
    step: &CodexEnvironmentInstallStep,
) -> CodexEnvironmentInstallLog {
    let Some(command) = install_command(
        platform,
        default_codex_home().ok().as_ref(),
        inventory,
        &step.id,
    ) else {
        return CodexEnvironmentInstallLog {
            step_id: step.id.clone(),
            label: step.label.clone(),
            status: "unsupported".to_owned(),
            detail: "当前平台缺少可自动执行该步骤的包管理器或授权代理。".to_owned(),
            command: step.command.clone(),
            next_action: Some(step.command.as_deref().map_or(
                "请查看检查卡片中的手动命令。".to_owned(),
                |command| format!("请手动执行：{command}"),
            )),
        };
    };
    let result = if step.id == STEP_CODEX_HOME {
        default_codex_home()
            .and_then(|path| fs::create_dir_all(path).map_err(|_| AppError::RuntimeUnavailable))
            .map(|()| CommandResult {
                success: true,
                stdout: "Codex home 目录已创建或已存在。".to_owned(),
                stderr: String::new(),
                code: Some(0),
            })
            .map_err(|error| error.to_string())
    } else {
        executor.output(&command)
    };
    match result {
        Ok(output) if output.success => CodexEnvironmentInstallLog {
            step_id: step.id.clone(),
            label: step.label.clone(),
            status: "completed".to_owned(),
            detail: non_empty_detail(&output),
            command: Some(command.display),
            next_action: command
                .next_action
                .or_else(|| Some("重新检查环境状态。".to_owned())),
        },
        Ok(output) => {
            let detail = non_empty_detail(&output);
            let (status, next_action) = install_failure_action(platform, &command, &detail);
            CodexEnvironmentInstallLog {
                step_id: step.id.clone(),
                label: step.label.clone(),
                status,
                detail,
                command: Some(command.display),
                next_action: Some(next_action),
            }
        }
        Err(error) => CodexEnvironmentInstallLog {
            step_id: step.id.clone(),
            label: step.label.clone(),
            status: "failed".to_owned(),
            detail: error,
            command: Some(command.display),
            next_action: Some("请确认命令存在且应用有权限启动该命令。".to_owned()),
        },
    }
}

fn install_failure_action(
    platform: EnvironmentPlatform,
    command: &CommandSpec,
    detail: &str,
) -> (String, String) {
    let lower = detail.to_ascii_lowercase();
    if command.requires_privilege || lower.contains("permission") || lower.contains("eacces") {
        let manual = if platform == EnvironmentPlatform::Windows {
            format!("请以管理员身份打开终端后执行：{}", command.display)
        } else if command.display.starts_with("npm ") {
            format!("请在终端执行：sudo {}", command.display)
        } else {
            format!("请在终端手动执行：{}", command.display)
        };
        return ("needs_privilege".to_owned(), manual);
    }
    (
        "failed".to_owned(),
        "请根据日志修复包管理器或网络问题后重试。".to_owned(),
    )
}

fn non_empty_detail(output: &CommandResult) -> String {
    let detail = concise_command_output(&output.stdout, &output.stderr);
    if detail.is_empty() {
        if output.success {
            "命令已完成。".to_owned()
        } else {
            format!(
                "命令退出码：{}。",
                output
                    .code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            )
        }
    } else {
        detail
    }
}

fn ca_verify_command(
    platform: EnvironmentPlatform,
    inventory: &ToolInventory,
    path: &Path,
) -> Option<CommandSpec> {
    let path_text = path.display().to_string();
    match platform {
        EnvironmentPlatform::Macos if inventory.security => Some(CommandSpec::new(
            "/usr/bin/security",
            &["verify-cert", "-c", &path_text],
            &format!("security verify-cert -c {path_text}"),
        )),
        EnvironmentPlatform::Windows if inventory.certutil => Some(CommandSpec::new(
            "certutil.exe",
            &["-user", "-verify", &path_text],
            &format!("certutil.exe -user -verify {path_text}"),
        )),
        EnvironmentPlatform::Linux if inventory.openssl => Some(CommandSpec::new(
            "openssl",
            &["verify", "-CApath", "/etc/ssl/certs", &path_text],
            &format!("openssl verify -CApath /etc/ssl/certs {path_text}"),
        )),
        _ => None,
    }
}

fn ca_trust_manual_command(platform: EnvironmentPlatform, path: &Path) -> Option<String> {
    let path_text = path.display();
    match platform {
        EnvironmentPlatform::Macos => Some(format!(
            "security add-trusted-cert -d -r trustRoot -k ~/Library/Keychains/login.keychain-db {path_text}"
        )),
        EnvironmentPlatform::Windows => Some(format!(
            "certutil.exe -user -addstore Root {path_text}"
        )),
        EnvironmentPlatform::Linux => Some(format!(
            "sudo cp {path_text} /usr/local/share/ca-certificates/codex-relay-gateway-ca.crt && sudo update-ca-certificates"
        )),
        EnvironmentPlatform::Other => None,
    }
}

fn codex_home_command(platform: EnvironmentPlatform, path: Option<&Path>) -> String {
    let fallback = if platform == EnvironmentPlatform::Windows {
        "%USERPROFILE%\\.codex".to_owned()
    } else {
        "$HOME/.codex".to_owned()
    };
    let path = path
        .map(|path| path.display().to_string())
        .unwrap_or(fallback);
    if platform == EnvironmentPlatform::Windows {
        format!("mkdir {path}")
    } else {
        format!("mkdir -p {path}")
    }
}

fn port_diagnostic_command(platform: EnvironmentPlatform) -> String {
    match platform {
        EnvironmentPlatform::Windows => format!("netstat -ano | findstr :{OAUTH_CALLBACK_PORT}"),
        EnvironmentPlatform::Macos | EnvironmentPlatform::Linux => {
            format!("lsof -nP -iTCP:{OAUTH_CALLBACK_PORT} -sTCP:LISTEN")
        }
        EnvironmentPlatform::Other => format!("检查 127.0.0.1:{OAUTH_CALLBACK_PORT} 占用进程"),
    }
}

fn browser_fix_command(platform: EnvironmentPlatform) -> String {
    match platform {
        EnvironmentPlatform::Windows => "start ms-settings:defaultapps".to_owned(),
        EnvironmentPlatform::Macos => {
            "open 'x-apple.systempreferences:com.apple.Desktop-Settings.extension'".to_owned()
        }
        EnvironmentPlatform::Linux => {
            "sudo apt-get install -y xdg-utils 或安装系统默认浏览器".to_owned()
        }
        EnvironmentPlatform::Other => "安装并配置系统默认浏览器".to_owned(),
    }
}

fn browser_detail(platform: EnvironmentPlatform) -> String {
    match platform {
        EnvironmentPlatform::Windows => {
            "已检测到 Windows URL 打开入口（rundll32/explorer）。".to_owned()
        }
        EnvironmentPlatform::Macos => "已检测到 macOS open 命令。".to_owned(),
        EnvironmentPlatform::Linux => "已检测到 Linux URL 打开入口。".to_owned(),
        EnvironmentPlatform::Other => "已检测到浏览器入口。".to_owned(),
    }
}

fn platform_display(platform: EnvironmentPlatform) -> &'static str {
    match platform {
        EnvironmentPlatform::Windows => "Windows",
        EnvironmentPlatform::Macos => "macOS",
        EnvironmentPlatform::Linux => "Linux",
        EnvironmentPlatform::Other => "当前平台",
    }
}

fn shell_word(command: &str) -> String {
    if command
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        command.to_owned()
    } else {
        format!("'{}'", command.replace('\'', "'\\''"))
    }
}

fn concise_command_output(stdout: &str, stderr: &str) -> String {
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    let mut line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned();
    if line.len() > 300 {
        line.truncate(300);
        line.push('…');
    }
    line
}

fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeExecutor {
        responses: HashMap<String, CommandResult>,
    }

    impl FakeExecutor {
        fn with(mut self, program: &str, args: &[&str], success: bool, stdout: &str) -> Self {
            let key = key(program, args);
            self.responses.insert(
                key,
                CommandResult {
                    success,
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                    code: Some(if success { 0 } else { 1 }),
                },
            );
            self
        }
    }

    impl CommandExecutor for FakeExecutor {
        fn output(&self, command: &CommandSpec) -> Result<CommandResult, String> {
            self.responses
                .get(&key(
                    &command.program,
                    &command.args.iter().map(String::as_str).collect::<Vec<_>>(),
                ))
                .cloned()
                .ok_or_else(|| format!("{} not found", command.program))
        }
    }

    fn key(program: &str, args: &[&str]) -> String {
        format!("{} {}", program, args.join(" "))
    }

    #[test]
    fn resolves_default_codex_home_for_macos_and_windows() {
        assert_eq!(
            default_codex_home_from_values(
                false,
                Some(PathBuf::from("/Users/example")),
                Some(PathBuf::from("ignored")),
                None,
                None,
            )
            .unwrap(),
            PathBuf::from("/Users/example/.codex")
        );
        assert_eq!(
            default_codex_home_from_values(
                true,
                Some(PathBuf::from("ignored")),
                Some(PathBuf::from(r"C:\Users\example")),
                None,
                None,
            )
            .unwrap(),
            PathBuf::from(r"C:\Users\example/.codex")
        );
    }

    #[test]
    fn reports_oauth_port_check() {
        let check = oauth_port_check(EnvironmentPlatform::Macos);
        assert_eq!(check.id, "oauth_callback_port");
        assert!(matches!(check.status.as_str(), "ok" | "warning"));
    }

    #[test]
    fn linux_install_steps_use_pkexec_when_available() {
        let inventory = ToolInventory {
            pkexec: true,
            apt_get: true,
            npm: true,
            ..ToolInventory::default()
        };
        let home = PathBuf::from("/home/dev/.codex");
        let steps = install_steps(EnvironmentPlatform::Linux, Some(&home), &inventory);
        let node = steps.iter().find(|step| step.id == STEP_NODE).unwrap();
        assert!(node.available);
        assert!(node.requires_privilege);
        assert!(node.command.as_deref().unwrap().contains("pkexec sh -lc"));
        let codex = steps.iter().find(|step| step.id == STEP_CODEX_CLI).unwrap();
        assert_eq!(
            codex.command.as_deref(),
            Some("npm install --global @openai/codex")
        );
    }

    #[test]
    fn macos_install_steps_prefer_homebrew() {
        let inventory = ToolInventory {
            brew: true,
            npm: true,
            ..ToolInventory::default()
        };
        let home = PathBuf::from("/Users/dev/.codex");
        let steps = install_steps(EnvironmentPlatform::Macos, Some(&home), &inventory);
        assert_eq!(
            steps
                .iter()
                .find(|step| step.id == STEP_NODE)
                .unwrap()
                .command
                .as_deref(),
            Some("brew install node")
        );
        assert_eq!(
            steps
                .iter()
                .find(|step| step.id == STEP_GIT)
                .unwrap()
                .command
                .as_deref(),
            Some("brew install git")
        );
    }

    #[test]
    fn partial_install_returns_step_logs_without_aborting() {
        let executor = FakeExecutor::default()
            .with(
                "sh",
                &["-lc", "command -v apt-get"],
                true,
                "/usr/bin/apt-get",
            )
            .with("sh", &["-lc", "command -v pkexec"], true, "/usr/bin/pkexec")
            .with("sh", &["-lc", "command -v npm"], true, "/usr/bin/npm")
            .with(
                "pkexec",
                &[
                    "sh",
                    "-lc",
                    "apt-get update && apt-get install -y nodejs npm",
                ],
                false,
                "permission denied",
            )
            .with(
                "npm",
                &["install", "--global", "@openai/codex"],
                true,
                "installed",
            );
        let input = InstallCodexEnvironmentInput {
            confirmed: true,
            steps: Some(vec![STEP_NODE.to_owned(), STEP_CODEX_CLI.to_owned()]),
        };
        let report =
            install_with_executor(input, &executor, EnvironmentPlatform::Linux, None).unwrap();
        assert_eq!(report.status, "failed");
        assert!(report
            .logs
            .iter()
            .any(|log| log.status == "needs_privilege"));
        assert!(report.logs.iter().any(|log| log.status == "completed"));
    }
}
