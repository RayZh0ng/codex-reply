import { Browser } from "@phosphor-icons/react/Browser";
import { CheckCircle } from "@phosphor-icons/react/CheckCircle";
import { Desktop } from "@phosphor-icons/react/Desktop";
import { Moon } from "@phosphor-icons/react/Moon";
import { ShieldCheck } from "@phosphor-icons/react/ShieldCheck";
import { Sun } from "@phosphor-icons/react/Sun";
import { TerminalWindow } from "@phosphor-icons/react/TerminalWindow";
import { Trash } from "@phosphor-icons/react/Trash";
import { Warning } from "@phosphor-icons/react/Warning";
import { Wrench } from "@phosphor-icons/react/Wrench";
import { XCircle } from "@phosphor-icons/react/XCircle";

import {
  type AppUpdateChannel,
  type AppUpdateInfo,
  type AppUpdateProgressEvent,
  type AppUpdateSettings,
  type CodexEnvironmentInstallReport,
  type CodexEnvironmentReport,
  type DesktopWorkspaceHistoryItem,
} from "../../shared/ipc";
import type { ThemePreference } from "../../shared/theme";

const themes: Array<{
  value: ThemePreference;
  label: string;
  icon: typeof Desktop;
}> = [
  { value: "system", label: "跟随系统", icon: Desktop },
  { value: "light", label: "浅色", icon: Sun },
  { value: "dark", label: "深色", icon: Moon },
];

const updateChannels: Array<{
  value: AppUpdateChannel;
  label: string;
  detail: string;
}> = [
  {
    value: "stable",
    label: "稳定版",
    detail: "只接收正式发布版本。",
  },
  {
    value: "beta",
    label: "Beta 版",
    detail: "提前接收预发布版本。",
  },
];

interface SettingsProps {
  workspaces: DesktopWorkspaceHistoryItem[];
  busy: boolean;
  themePreference: ThemePreference;
  updateSettings: AppUpdateSettings;
  availableUpdate: AppUpdateInfo | null;
  updateStatus: string | null;
  updateBusy: boolean;
  updateProgress: AppUpdateProgressEvent | null;
  codexEnvironment: CodexEnvironmentReport | null;
  codexEnvironmentInstall: CodexEnvironmentInstallReport | null;
  codexEnvironmentBusy: boolean;
  onThemePreferenceChange: (preference: ThemePreference) => void;
  onChangeUpdateSettings: (settings: AppUpdateSettings) => Promise<void>;
  onCheckUpdate: () => Promise<void>;
  onInstallUpdate: () => Promise<void>;
  onRefreshCodexEnvironment: () => Promise<void>;
  onInstallCodexEnvironment: () => Promise<void>;
  onDelete: (id: string, alias: string) => void;
}

export function Settings({
  workspaces,
  busy,
  themePreference,
  updateSettings,
  availableUpdate,
  updateStatus,
  updateBusy,
  updateProgress,
  codexEnvironment,
  codexEnvironmentInstall,
  codexEnvironmentBusy,
  onThemePreferenceChange,
  onChangeUpdateSettings,
  onCheckUpdate,
  onInstallUpdate,
  onRefreshCodexEnvironment,
  onInstallCodexEnvironment,
  onDelete,
}: SettingsProps) {
  const installableMissingIds = new Set(
    codexEnvironment?.install_steps
      .filter((step) => step.available)
      .map((step) => step.id) ?? [],
  );
  const hasInstallableMissing =
    codexEnvironment?.checks.some(
      (check) => check.status === "missing" && installableMissingIds.has(check.id),
    ) ?? false;
  const hasManualCommands = Boolean(codexEnvironment?.manual_commands.length);
  const canAutoInstall = Boolean(
    codexEnvironment?.can_install && hasInstallableMissing,
  );
  const updateInProgress = Boolean(updateProgress && updateProgress.phase !== "failed");
  const updateControlsDisabled = busy || updateBusy || updateInProgress;
  const environmentTone = codexEnvironment
    ? environmentSummaryTone(codexEnvironment.summary.status)
    : "neutral";
  const copyManualCommands = () => {
    const commands = codexEnvironment?.manual_commands ?? [];
    if (!commands.length) return;
    void navigator.clipboard?.writeText(commands.join("\n"));
  };

  return (
    <div className="page settings-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <h1>客户端工作区</h1>
          <p className="page-subtitle">管理应用外观与 Codex 档案的桌面工作区。</p>
        </div>
      </header>
      <section className="surface-card appearance-settings" data-animate="cards">
        <div className="card-heading">
          <div>
            <h2>外观</h2>
            <p>跟随系统，或为 Relay 单独选择浅色与深色主题。</p>
          </div>
        </div>
        <fieldset className="theme-picker" aria-label="应用主题">
          <legend className="sr-only">应用主题</legend>
          {themes.map((option) => {
            const Icon = option.icon;
            return (
              <label
                className={themePreference === option.value ? "selected" : ""}
                key={option.value}
              >
                <input
                  checked={themePreference === option.value}
                  name="application-theme"
                  onChange={() => onThemePreferenceChange(option.value)}
                  type="radio"
                  value={option.value}
                />
                <Icon
                  size={17}
                  weight={themePreference === option.value ? "fill" : "regular"}
                />
                <span>{option.label}</span>
              </label>
            );
          })}
        </fieldset>
      </section>
      <section className="surface-card update-settings" data-animate="cards">
        <div className="card-heading">
          <div>
            <h2>软件更新</h2>
            <p>通过 GitHub Release 检查 stable / beta 双通道更新。</p>
          </div>
        </div>
        <fieldset className="theme-picker update-channel-picker" aria-label="更新通道">
          <legend className="sr-only">更新通道</legend>
          {updateChannels.map((option) => (
            <label
              className={updateSettings.channel === option.value ? "selected" : ""}
              key={option.value}
            >
              <input
                checked={updateSettings.channel === option.value}
                disabled={updateControlsDisabled}
                name="app-update-channel"
                onChange={() =>
                  void onChangeUpdateSettings({
                    ...updateSettings,
                    channel: option.value,
                  })
                }
                type="radio"
                value={option.value}
              />
              <span>{option.label}</span>
              <small>{option.detail}</small>
            </label>
          ))}
        </fieldset>
        <label className="update-toggle">
          <input
            checked={updateSettings.auto_check}
            disabled={updateControlsDisabled}
            onChange={(event) =>
              void onChangeUpdateSettings({
                ...updateSettings,
                auto_check: event.target.checked,
              })
            }
            type="checkbox"
          />
          <span>启动时自动检查更新</span>
        </label>
        {availableUpdate ? (
          <article className="update-available-panel" role="status">
            <strong>
              发现 {formatUpdateChannel(availableUpdate.channel)}
              更新：{availableUpdate.version}
            </strong>
            <p>
              当前版本 {availableUpdate.current_version}
              {availableUpdate.date ? ` · 发布于 ${availableUpdate.date}` : ""}
            </p>
            {availableUpdate.body && <p>{availableUpdate.body}</p>}
          </article>
        ) : (
          updateStatus && <p className="form-note">{updateStatus}</p>
        )}
        {updateProgress && <UpdateProgressPanel progress={updateProgress} />}
        <div className="update-actions">
          <button
            className="quiet-button"
            disabled={updateControlsDisabled}
            onClick={() => void onCheckUpdate()}
            type="button"
          >
            {updateInProgress ? "正在更新…" : updateBusy ? "正在检查…" : "立即检查更新"}
          </button>
          {availableUpdate && (
            <button
              className="primary-button"
              disabled={updateControlsDisabled}
              onClick={() => void onInstallUpdate()}
              type="button"
            >
              {updateInProgress ? "正在更新…" : "安装并重启"}
            </button>
          )}
        </div>
      </section>
      <section
        className={`surface-card codex-environment-settings environment-${environmentTone}`}
        data-animate="cards"
      >
        <div className="environment-hero">
          <div className="environment-hero-copy">
            <p className="section-kicker">Environment</p>
            <h2>Codex 环境检查</h2>
            <p>
              覆盖 Windows / macOS / Linux：Node.js LTS、npm、Codex CLI、Git、 Codex
              home、OAuth 回调、默认浏览器与 Relay CA。
            </p>
          </div>
          {codexEnvironment ? (
            <div className={`environment-score-card is-${environmentTone}`}>
              <span>{codexEnvironment.summary.health_percent}</span>
              <small>健康分</small>
            </div>
          ) : (
            <div className="environment-score-card is-neutral">
              <span>--</span>
              <small>未检查</small>
            </div>
          )}
        </div>
        {codexEnvironment ? (
          <>
            <article className={`environment-summary-panel is-${environmentTone}`}>
              <EnvironmentSummaryIcon status={codexEnvironment.summary.status} />
              <div>
                <strong>{codexEnvironment.message}</strong>
                <p>
                  平台：{formatEnvironmentPlatform(codexEnvironment.platform)}
                  {codexEnvironment.codex_home
                    ? ` · Codex home：${codexEnvironment.codex_home}`
                    : ""}
                  {codexEnvironment.last_checked_at_ms
                    ? ` · 上次检查：${formatTimestamp(codexEnvironment.last_checked_at_ms)}`
                    : ""}
                </p>
              </div>
              <div className="environment-summary-metrics" aria-label="环境检查统计">
                <span>{codexEnvironment.summary.ok_count} 正常</span>
                <span>{codexEnvironment.summary.warning_count} 提示</span>
                <span>{codexEnvironment.summary.missing_count} 缺失</span>
                <span>{codexEnvironment.summary.failed_count} 失败</span>
              </div>
            </article>
            <ul className="environment-check-grid" aria-label="Codex 环境检查结果">
              {codexEnvironment.checks.map((check) => (
                <li
                  key={check.id}
                  className={`environment-check-card is-${check.status}`}
                >
                  <div className="environment-check-topline">
                    <span
                      className={`environment-check-icon ${environmentStatusTone(check.status)}`}
                    >
                      <EnvironmentCheckIcon id={check.id} status={check.status} />
                    </span>
                    <span
                      className={`status-pill compact ${environmentStatusTone(check.status)}`}
                    >
                      <i />
                      {environmentStatusLabel(check.status)}
                    </span>
                  </div>
                  <div className="environment-check-body">
                    <strong>{check.label}</strong>
                    {check.description && <small>{check.description}</small>}
                    <p>{check.detail}</p>
                  </div>
                  {(check.command || check.next_action) && (
                    <details className="environment-check-detail">
                      <summary>修复详情</summary>
                      {check.next_action && <span>{check.next_action}</span>}
                      {check.command && <code>{check.command}</code>}
                    </details>
                  )}
                </li>
              ))}
            </ul>
          </>
        ) : (
          <div className="environment-empty-state">
            <TerminalWindow size={24} weight="duotone" />
            <p>点击“重新检查”获取当前 Codex 本机环境状态。</p>
          </div>
        )}
        {codexEnvironmentInstall && (
          <details
            className="environment-install-report"
            open={codexEnvironmentInstall.status !== "completed"}
          >
            <summary>
              <strong>{codexEnvironmentInstall.message}</strong>
              <span>{codexEnvironmentInstall.logs.length} 个步骤</span>
            </summary>
            <ul>
              {codexEnvironmentInstall.logs.map((log) => (
                <li key={log.step_id} className={`is-${log.status}`}>
                  <div>
                    <strong>{log.label}</strong>
                    <span>
                      {environmentStatusLabel(log.status)} · {log.detail}
                    </span>
                    {log.next_action && <em>{log.next_action}</em>}
                    {log.command && <code>{log.command}</code>}
                  </div>
                </li>
              ))}
            </ul>
          </details>
        )}
        <div className="environment-actions">
          <button
            className="quiet-button"
            disabled={busy || codexEnvironmentBusy}
            onClick={() => void onRefreshCodexEnvironment()}
            type="button"
          >
            {codexEnvironmentBusy ? "正在检查…" : "重新检查"}
          </button>
          <button
            className="primary-button"
            disabled={busy || codexEnvironmentBusy || !canAutoInstall}
            onClick={() => void onInstallCodexEnvironment()}
            type="button"
          >
            {codexEnvironmentBusy ? "正在部署…" : "一键部署缺失项"}
          </button>
          <button
            className="quiet-button"
            disabled={busy || codexEnvironmentBusy || !hasManualCommands}
            onClick={copyManualCommands}
            type="button"
          >
            复制修复命令
          </button>
        </div>
      </section>
      <section className="surface-card workspace-history">
        <div className="card-heading">
          <div>
            <h2>旧独立工作区清理</h2>
          </div>
        </div>
        <p className="muted-copy">
          已取消独立工作区模式。这里仅列出历史遗留的全新工作区，可按需清理其本地客户端数据。
        </p>
        {workspaces.length ? (
          <ul>
            {workspaces.map((workspace) => (
              <li key={workspace.id}>
                <div>
                  <strong>{workspace.profile_alias}</strong>
                  <span>
                    创建于 {formatTimestamp(workspace.created_at_ms)} · 最近使用{" "}
                    {formatTimestamp(workspace.last_launched_at_ms)}
                  </span>
                </div>
                <div className="workspace-history-actions">
                  <button
                    className="icon-button danger"
                    disabled={busy}
                    aria-label={`删除 ${workspace.profile_alias} 的旧独立工作区`}
                    onClick={() => onDelete(workspace.id, workspace.profile_alias)}
                    type="button"
                  >
                    <Trash size={17} />
                  </button>
                </div>
              </li>
            ))}
          </ul>
        ) : (
          <p className="muted-copy">当前没有可清理的旧独立工作区。</p>
        )}
      </section>
    </div>
  );
}

function UpdateProgressPanel({ progress }: { progress: AppUpdateProgressEvent }) {
  const percent = progress.progress_percent;
  return (
    <article className={`update-progress-panel is-${progress.phase}`} role="status">
      <div className="update-progress-meter-heading">
        <strong>{updateProgressPhaseLabel(progress.phase)}</strong>
        <span>
          {percent === null ? formatBytes(progress.downloaded_bytes) : `${percent}%`}
        </span>
      </div>
      <progress
        aria-label="更新进度"
        max={100}
        value={percent === null ? undefined : percent}
      />
      <p>{progress.message}</p>
      <small>{formatUpdateByteSummary(progress)}</small>
    </article>
  );
}

function updateProgressPhaseLabel(phase: AppUpdateProgressEvent["phase"]) {
  return (
    {
      checking: "准备下载",
      downloading: "正在下载",
      downloaded: "下载完成",
      installing: "正在安装",
      restarting: "准备重启",
      failed: "更新失败",
    }[phase] ?? phase
  );
}

function formatUpdateByteSummary(progress: AppUpdateProgressEvent) {
  if (progress.content_length && progress.content_length > 0) {
    return `${formatBytes(progress.downloaded_bytes)} / ${formatBytes(progress.content_length)}`;
  }
  if (progress.phase === "checking") return "正在连接更新服务…";
  return `${formatBytes(progress.downloaded_bytes)} 已下载`;
}

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let next = value;
  let unitIndex = 0;
  while (next >= 1024 && unitIndex < units.length - 1) {
    next /= 1024;
    unitIndex += 1;
  }
  const precision = unitIndex === 0 || Number.isInteger(next) || next >= 10 ? 0 : 1;
  return `${next.toFixed(precision)} ${units[unitIndex]}`;
}

function EnvironmentSummaryIcon({ status }: { status: string }) {
  if (status === "healthy") return <CheckCircle size={24} weight="fill" />;
  if (status === "action_required") return <XCircle size={24} weight="fill" />;
  return <Warning size={24} weight="fill" />;
}

function EnvironmentCheckIcon({ id, status }: { id: string; status: string }) {
  if (status === "ok") return <CheckCircle size={18} weight="fill" />;
  if (status === "failed" || status === "missing") {
    return <XCircle size={18} weight="fill" />;
  }
  if (id === "browser") return <Browser size={18} weight="duotone" />;
  if (id === "relay_ca") return <ShieldCheck size={18} weight="duotone" />;
  if (id === "codex_home") return <Wrench size={18} weight="duotone" />;
  return <TerminalWindow size={18} weight="duotone" />;
}

function formatTimestamp(value: number) {
  return new Intl.DateTimeFormat("zh-CN", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(value));
}

function formatUpdateChannel(channel: AppUpdateChannel) {
  return channel === "beta" ? "Beta" : "稳定版";
}

function environmentStatusLabel(status: string) {
  return (
    {
      ok: "正常",
      missing: "缺失",
      warning: "需处理",
      failed: "失败",
      completed: "已完成",
      skipped: "已跳过",
      needs_privilege: "需授权",
      unsupported: "需手动",
    }[status] ?? status
  );
}

function environmentStatusTone(status: string) {
  if (status === "ok" || status === "completed") return "success";
  if (status === "warning" || status === "needs_privilege") return "warning";
  if (status === "missing" || status === "failed" || status === "unsupported")
    return "danger";
  return "neutral";
}

function environmentSummaryTone(status: string) {
  if (status === "healthy") return "success";
  if (status === "warning") return "warning";
  if (status === "action_required") return "danger";
  return "neutral";
}

function formatEnvironmentPlatform(platform: string) {
  return (
    {
      windows: "Windows",
      macos: "macOS",
      linux: "Linux",
      other: "其它平台",
    }[platform] ?? platform
  );
}
