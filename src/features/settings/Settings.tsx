import { ArrowCounterClockwise, Trash, Warning } from "@phosphor-icons/react";

import {
  type DesktopWorkspaceHistoryItem,
  type DesktopWorkspaceMode,
  type DesktopWorkspaceSettings,
} from "../../shared/ipc";

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

interface SettingsProps {
  settings: DesktopWorkspaceSettings;
  workspaces: DesktopWorkspaceHistoryItem[];
  busy: boolean;
  onChangeMode: (mode: DesktopWorkspaceMode) => Promise<void>;
  onRestore: (id: string) => Promise<void>;
  onDelete: (id: string, alias: string) => void;
}

export function Settings({
  settings,
  workspaces,
  busy,
  onChangeMode,
  onRestore,
  onDelete,
}: SettingsProps) {
  return (
    <div className="page settings-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <p className="section-kicker">Desktop workspace</p>
          <h1>客户端工作区</h1>
          <p className="page-subtitle">
            选择切换 Codex 档案时如何使用 ChatGPT/Codex 桌面客户端。
          </p>
        </div>
      </header>
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
      <article className="privacy-banner" data-animate="notice">
        <Warning size={23} weight="fill" />
        <div>
          <strong>ChatGPT 账号数据不会跨账号迁移</strong>
          <p>
            Relay 不读取、复制或合并 ChatGPT Chat/Work 的
            Cookie、钥匙串、云端聊天或记忆；共享模式只复用原客户端已有的本地数据目录。
          </p>
        </div>
      </article>
      <section className="surface-card workspace-history">
        <div className="card-heading">
          <div>
            <p className="section-kicker">Fresh workspace history</p>
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
