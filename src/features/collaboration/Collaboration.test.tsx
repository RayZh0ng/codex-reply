import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Collaboration } from "./Collaboration";
import type { MaskedCollaborationBot, MaskedProfile } from "../../shared/ipc";

const dialog = vi.hoisted(() => ({ open: vi.fn() }));

vi.mock("@tauri-apps/plugin-dialog", () => dialog);

const bot: MaskedCollaborationBot = {
  id: "bot-1",
  provider: "feishu",
  name: "研发 Codex 助手",
  enabled: true,
  connection_status: "connected",
  credential_mask: "cli_••••test",
  config_summary: "App ID cli_••••test",
  callback_public_url: null,
  last_error: null,
  updated_at_ms: 1,
};

const profile: MaskedProfile = {
  id: "profile-1",
  alias: "工作账号",
  kind: "codex_oauth",
  base_url: null,
  enabled: true,
  in_pool: false,
  priority: 0,
  weight: 1,
  models: [],
  health: "unknown",
  cooldown_until_ms: null,
  credential_configured: true,
  is_current: true,
};

const handlers = () => ({
  onSaveBot: vi.fn().mockResolvedValue(undefined),
  onTestBot: vi.fn().mockResolvedValue(undefined),
  onDeleteBot: vi.fn(),
  onSaveBinding: vi.fn().mockResolvedValue(undefined),
  onDeleteBinding: vi.fn(),
  onRegisterDiscordCommands: vi.fn().mockResolvedValue(undefined),
  onLoadCallbackStatus: vi.fn().mockResolvedValue({
    local_url: "http://127.0.0.1:53821/collaboration/wecom/<bot_id>",
    public_urls: [],
    running: true,
  }),
  onCancelSession: vi.fn().mockResolvedValue(undefined),
  onContinueSession: vi.fn().mockResolvedValue(undefined),
  onRefresh: vi.fn().mockResolvedValue(undefined),
});

afterEach(() => {
  cleanup();
  dialog.open.mockReset();
});

describe("Collaboration", () => {
  it("guides users from zero and shows all providers as configurable entry points", () => {
    const props = handlers();

    render(
      <Collaboration
        bots={[]}
        bindings={[]}
        sessions={[]}
        profiles={[]}
        busy={false}
        {...props}
      />,
    );

    expect(screen.getByRole("heading", { name: "连接群聊" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "使用指南" }));
    expect(
      screen.getByRole("heading", { name: "从 0 创建并使用飞书连接器" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /飞书/ })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /QQ/ })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /企业微信/ })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /Discord/ })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /Telegram/ })).toBeInTheDocument();
    expect(screen.queryByText("即将支持")).not.toBeInTheDocument();
    expect(screen.getAllByText("未配置")).toHaveLength(5);
  });

  it("creates a project binding without rendering the absolute directory", async () => {
    const props = handlers();
    dialog.open.mockResolvedValueOnce("/private/workspace/codex-relay");

    render(
      <Collaboration
        bots={[bot]}
        bindings={[]}
        sessions={[]}
        profiles={[profile]}
        busy={false}
        {...props}
      />,
    );

    fireEvent.click(screen.getByRole("combobox", { name: "机器人" }));
    fireEvent.click(screen.getByRole("option", { name: /研发 Codex 助手/ }));
    fireEvent.click(screen.getByRole("combobox", { name: "Codex 档案" }));
    fireEvent.click(screen.getByRole("option", { name: "工作账号" }));
    fireEvent.change(screen.getByLabelText("项目名称"), {
      target: { value: "Codex Relay" },
    });
    fireEvent.click(screen.getByRole("button", { name: "选择目录" }));

    await waitFor(() => expect(screen.getByText("已选择工作目录")).toBeInTheDocument());
    expect(
      screen.queryByText("/private/workspace/codex-relay"),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "创建绑定" }));

    await waitFor(() => expect(props.onSaveBinding).toHaveBeenCalledOnce());
    expect(props.onSaveBinding).toHaveBeenCalledWith(
      expect.objectContaining({
        bot_id: "bot-1",
        profile_id: "profile-1",
        project_name: "Codex Relay",
        project_slug: "codex-relay",
        working_directory: "/private/workspace/codex-relay",
      }),
    );
  });
});
