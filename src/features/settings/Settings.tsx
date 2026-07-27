import { ArrowCounterClockwise } from "@phosphor-icons/react/ArrowCounterClockwise";
import { Desktop } from "@phosphor-icons/react/Desktop";
import { Moon } from "@phosphor-icons/react/Moon";
import { Sun } from "@phosphor-icons/react/Sun";
import { Trash } from "@phosphor-icons/react/Trash";
import { Warning } from "@phosphor-icons/react/Warning";

import {
  type DesktopWorkspaceHistoryItem,
  type DesktopWorkspaceMode,
  type DesktopWorkspaceSettings,
} from "../../shared/ipc";
import type { ThemePreference } from "../../shared/theme";

const modes: Array<{
  mode: DesktopWorkspaceMode;
  title: string;
  detail: string;
}> = [
  {
    mode: "fresh",
    title: "每次全新启动",
    detail: "每次切换创建空白工作区并保留，可从下方历史记录恢复。",
  },
  {
    mode: "per_profile",
    title: "账号独立工作区",
    detail: "每个 Relay 档案使用固定工作区，保留各自的客户端会话、状态与设置。",
  },
  {
    mode: "shared",
    title: "共享原客户端状态",
    detail:
      "使用原有 ChatGPT/Codex 数据目录，保留已保存的本地客户端状态。切换前会请求关闭客户端。",
  },
];

const themes: Array<{
  value: ThemePreference;
  label: string;
  icon: typeof Desktop;
}> = [
  { value: "system", label: "跟随系统", icon: Desktop },
  { value: "light", label: "浅色", icon: Sun },
  { value: "dark", label: "深色", icon: Moon },
];

interface SettingsProps {
  settings: DesktopWorkspaceSettings;
  workspaces: DesktopWorkspaceHistoryItem[];
  busy: boolean;
  themePreference: ThemePreference;
  onChangeMode: (mode: DesktopWorkspaceMode) => Promise<void>;
  onThemePreferenceChange: (preference: ThemePreference) => void;
  onRestore: (id: string) => Promise<void>;
  onDelete: (id: string, alias: string) => void;
}

export function Settings({
  settings,
  workspaces,
  busy,
  themePreference,
  onChangeMode,
  onThemePreferenceChange,
  onRestore,
  onDelete,
}: SettingsProps) {
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
      <div className="section-heading compact-section-heading">
        <div>
          <h2>工作区模式</h2>
          <p>选择切换档案时桌面客户端使用的本机状态。</p>
        </div>
      </div>
      <section
        className="workspace-mode-grid"
        data-animate="cards"
        aria-label="桌面工作区模式"
      >
        {modes.map((option) => (
          <label
            className={`workspace-mode-card ${settings.mode === option.mode ? "selected" : ""}`}
            key={option.mode}
          >
            <input
              checked={settings.mode === option.mode}
              disabled={busy}
              name="workspace-mode"
              onChange={() => void onChangeMode(option.mode)}
              type="radio"
              value={option.mode}
            />
            <strong>{option.title}</strong>
            <span>{option.detail}</span>
          </label>
        ))}
      </section>
      {settings.mode === "shared" && (
        <article className="privacy-banner contextual-notice" data-animate="notice">
          <Warning size={20} weight="fill" />
          <div>
            <strong>共享模式只复用原客户端的本机状态</strong>
            <p>账号 Cookie、钥匙串、云端聊天与记忆不会在档案之间迁移。</p>
          </div>
        </article>
      )}
      <section className="surface-card workspace-history">
        <div className="card-heading">
          <div>
            <h2>全新工作区历史</h2>
          </div>
        </div>
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
                    className="quiet-button"
                    disabled={busy || workspace.profile_alias === "已删除档案"}
                    onClick={() => void onRestore(workspace.id)}
                    type="button"
                  >
                    <ArrowCounterClockwise size={17} /> 恢复
                  </button>
                  <button
                    className="icon-button danger"
                    disabled={busy}
                    aria-label={`删除 ${workspace.profile_alias} 的工作区`}
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
          <p className="muted-copy">
            选择“每次全新启动”并切换账号后，保存的工作区会显示在这里。
          </p>
        )}
      </section>
    </div>
  );
}

function formatTimestamp(value: number) {
  return new Intl.DateTimeFormat("zh-CN", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(value));
}
