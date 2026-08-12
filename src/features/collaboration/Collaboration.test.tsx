import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Collaboration } from "./Collaboration";
import type {
  CodexSessionSummary,
  CollaborationContextSummary,
  CollaborationProjectBinding,
  MaskedCollaborationBot,
  MaskedProfile,
} from "../../shared/ipc";

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
  system_prompt: null,
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
  codex_oauth_profile_id: null,
  health: "unknown",
  cooldown_until_ms: null,
  credential_configured: true,
  auth_mode: "oauth",
  is_current: true,
  validation_status: "unknown",
  validated_at_ms: null,
  validation_message: null,
};

const longBinding: CollaborationProjectBinding = {
  id: "binding-long",
  provider: "feishu",
  bot_id: "bot-1",
  bot_name: "研发 Codex 助手",
  project_name: "一个非常非常长的项目名称用于验证协作页卡片不会因为标题过长而横向溢出",
  project_slug: "very-long-project-slug-that-should-wrap-or-truncate-in-card",
  working_directory: "/private/workspace/that/should/not/render",
  profile_id: "profile-1",
  profile_alias: "一个名字同样非常长的 Codex 工作账号档案",
  chat_id: "chat-1",
  bind_code: "ABCDEFG123",
  enabled: true,
  concurrency_limit: 2,
  execution_target: "profile",
  model_id: null,
  created_at_ms: 1,
  updated_at_ms: 2,
};

const longSession: CodexSessionSummary = {
  id: "session-long-1234567890",
  binding_id: "binding-long",
  context_id: "ctx-long-1234567890",
  provider: "feishu",
  provider_bot_id: "bot-1",
  provider_chat_id: "chat-1",
  provider_message_id: "message-1",
  project_name: longBinding.project_name,
  project_slug: longBinding.project_slug,
  profile_id: "profile-1",
  profile_alias: longBinding.profile_alias,
  relay_status: "completed",
  codex_session_id: "codex-session-long-1234567890",
  feishu_message_id: "message-1",
  feishu_chat_id: "chat-1",
  started_by: "sender-1",
  started_at_ms: 1,
  updated_at_ms: 2,
  finished_at_ms: 3,
  summary:
    "这是一段很长很长的最终摘要，用于验证会话列表在摘要内容较长时仍然保持卡片布局稳定并在窄宽度下自然换行或截断。",
  last_error: null,
  execution_target: "profile",
  model_id: null,
  turn_kind: "natural",
  conversation_mode: "default",
  goal_status: "active",
};

const longContext: CollaborationContextSummary = {
  id: "ctx-long-1234567890",
  scope_key:
    "dir=/private/workspace/that/should/not/render|target=profile|profile=profile-1|model=",
  binding_id: longBinding.id,
  provider: "feishu",
  bot_id: "bot-1",
  bot_name: bot.name,
  project_name: longBinding.project_name,
  project_slug: longBinding.project_slug,
  working_directory: longBinding.working_directory,
  execution_target: "profile",
  profile_id: "profile-1",
  profile_alias: longBinding.profile_alias,
  model_id: null,
  memory_enabled: true,
  permissions_policy: "workspace-write",
  active_codex_session_id: "codex-session-long-1234567890",
  active_relay_session_id: null,
  goal_status: "active",
  goal_text: "完成 beta 发布",
  conversation_mode: "default",
  last_turn_at_ms: 2,
  created_at_ms: 1,
  updated_at_ms: 2,
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
  onUpdateContext: vi.fn().mockResolvedValue(undefined),
  onResetContext: vi.fn().mockResolvedValue(undefined),
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
        contexts={[]}
        profiles={[]}
        gatewayModelOptions={[]}
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
        contexts={[]}
        profiles={[profile]}
        gatewayModelOptions={[]}
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
        execution_target: "profile",
        model_id: null,
      }),
    );
  });

  it("submits a bot-level system prompt when creating a collaboration bot", async () => {
    const props = handlers();

    render(
      <Collaboration
        bots={[]}
        bindings={[]}
        sessions={[]}
        contexts={[]}
        profiles={[]}
        gatewayModelOptions={[]}
        busy={false}
        {...props}
      />,
    );

    fireEvent.change(screen.getByLabelText("机器人名称"), {
      target: { value: "评审助手" },
    });
    fireEvent.change(screen.getByLabelText("App ID"), {
      target: { value: "cli_test" },
    });
    fireEvent.change(screen.getByLabelText("App Secret"), {
      target: { value: "secret" },
    });
    fireEvent.change(screen.getByLabelText("机器人专属提示词（可选）"), {
      target: { value: "回复必须包含变更文件和验证命令。" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存机器人" }));

    await waitFor(() => expect(props.onSaveBot).toHaveBeenCalledOnce());
    expect(props.onSaveBot).toHaveBeenCalledWith(
      expect.objectContaining({
        provider: "feishu",
        name: "评审助手",
        app_id: "cli_test",
        app_secret: "secret",
        system_prompt: "回复必须包含变更文件和验证命令。",
      }),
    );
  });

  it("edits an existing bot prompt without resubmitting masked secrets", async () => {
    const props = handlers();

    render(
      <Collaboration
        bots={[{ ...bot, system_prompt: "旧提示词" }]}
        bindings={[]}
        sessions={[]}
        contexts={[]}
        profiles={[profile]}
        gatewayModelOptions={[]}
        busy={false}
        {...props}
      />,
    );

    expect(screen.getByText("已配置专属提示词")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "编辑提示词" }));
    fireEvent.change(screen.getByLabelText("机器人专属提示词"), {
      target: { value: "新的专属提示词" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存提示词" }));

    await waitFor(() => expect(props.onSaveBot).toHaveBeenCalledOnce());
    expect(props.onSaveBot).toHaveBeenCalledWith({
      id: "bot-1",
      provider: "feishu",
      name: "研发 Codex 助手",
      enabled: true,
      confirmed: true,
      system_prompt: "新的专属提示词",
    });
    expect(props.onSaveBot.mock.calls[0]?.[0]).not.toHaveProperty("app_secret");
  });

  it("creates a gateway-backed project binding with a selected model", async () => {
    const props = handlers();
    dialog.open.mockResolvedValueOnce("/private/workspace/codex-relay");
    const gatewayProfile: MaskedProfile = {
      ...profile,
      in_pool: true,
      models: ["third-party-coder"],
      health: "healthy",
    };

    render(
      <Collaboration
        bots={[bot]}
        bindings={[]}
        sessions={[]}
        contexts={[]}
        profiles={[gatewayProfile]}
        gatewayModelOptions={["third-party-coder"]}
        busy={false}
        {...props}
      />,
    );

    fireEvent.click(screen.getByRole("combobox", { name: "机器人" }));
    fireEvent.click(screen.getByRole("option", { name: /研发 Codex 助手/ }));
    fireEvent.click(screen.getByRole("combobox", { name: "执行方式" }));
    fireEvent.click(screen.getByRole("option", { name: /API 服务网关/ }));
    expect(
      screen.queryByRole("combobox", { name: "Codex 档案" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("combobox", { name: "默认模型" }));
    fireEvent.click(screen.getByRole("option", { name: "third-party-coder" }));
    fireEvent.change(screen.getByLabelText("项目名称"), {
      target: { value: "Gateway Project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "选择目录" }));

    await waitFor(() => expect(screen.getByText("已选择工作目录")).toBeInTheDocument());
    fireEvent.click(screen.getByRole("button", { name: "创建绑定" }));

    await waitFor(() => expect(props.onSaveBinding).toHaveBeenCalledOnce());
    expect(props.onSaveBinding).toHaveBeenCalledWith(
      expect.objectContaining({
        execution_target: "gateway",
        profile_id: null,
        model_id: "third-party-coder",
      }),
    );
  });

  it("filters gateway model options to backend-eligible profiles", async () => {
    const props = handlers();
    const healthyGatewayProfile: MaskedProfile = {
      ...profile,
      id: "profile-healthy",
      alias: "健康账号",
      in_pool: true,
      models: ["usable-coder"],
      health: "healthy",
    };
    const unhealthyGatewayProfile: MaskedProfile = {
      ...profile,
      id: "profile-unhealthy",
      alias: "异常账号",
      in_pool: true,
      models: ["hidden-coder"],
      health: "unhealthy",
    };

    render(
      <Collaboration
        bots={[bot]}
        bindings={[]}
        sessions={[]}
        contexts={[]}
        profiles={[healthyGatewayProfile, unhealthyGatewayProfile]}
        gatewayModelOptions={["usable-coder"]}
        busy={false}
        {...props}
      />,
    );

    fireEvent.click(screen.getByRole("combobox", { name: "执行方式" }));
    fireEvent.click(screen.getByRole("option", { name: /API 服务网关/ }));
    fireEvent.click(screen.getByRole("combobox", { name: "默认模型" }));

    expect(screen.getByRole("option", { name: "usable-coder" })).toBeInTheDocument();
    expect(
      screen.queryByRole("option", { name: "hidden-coder" }),
    ).not.toBeInTheDocument();
  });

  it("requires an ASCII command slug when the project name cannot generate one", async () => {
    const props = handlers();
    dialog.open.mockResolvedValueOnce("/private/workspace/codex-relay");

    render(
      <Collaboration
        bots={[bot]}
        bindings={[]}
        sessions={[]}
        contexts={[]}
        profiles={[profile]}
        gatewayModelOptions={[]}
        busy={false}
        {...props}
      />,
    );

    fireEvent.click(screen.getByRole("combobox", { name: "机器人" }));
    fireEvent.click(screen.getByRole("option", { name: /研发 Codex 助手/ }));
    fireEvent.click(screen.getByRole("combobox", { name: "Codex 档案" }));
    fireEvent.click(screen.getByRole("option", { name: "工作账号" }));
    fireEvent.change(screen.getByLabelText("项目名称"), {
      target: { value: "中文项目" },
    });
    fireEvent.click(screen.getByRole("button", { name: "选择目录" }));

    await waitFor(() => expect(screen.getByText("已选择工作目录")).toBeInTheDocument());
    expect(screen.getByText(/群命令项目名需要/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "创建绑定" })).toBeDisabled();
    expect(props.onSaveBinding).not.toHaveBeenCalled();
  });

  it("renders long collaboration binding and session text without exposing the working directory", () => {
    const props = handlers();

    render(
      <Collaboration
        bots={[bot]}
        bindings={[longBinding]}
        sessions={[longSession]}
        contexts={[longContext]}
        profiles={[profile]}
        gatewayModelOptions={[]}
        busy={false}
        {...props}
      />,
    );

    expect(screen.getAllByText(longBinding.project_name).length).toBeGreaterThan(0);
    expect(
      screen.getAllByText(/very-long-project-slug-that-should-wrap/).length,
    ).toBeGreaterThan(0);
    expect(
      screen.getAllByText(/一个名字同样非常长的 Codex 工作账号档案/).length,
    ).toBeGreaterThan(0);
    expect(screen.getByText(/这是一段很长很长的最终摘要/)).toBeInTheDocument();
    expect(screen.getByText("项目协作上下文")).toBeInTheDocument();
    expect(screen.getByText(/完成 beta 发布/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "关闭记忆" }));
    expect(props.onUpdateContext).toHaveBeenCalledWith({
      context_id: longContext.id,
      memory_enabled: false,
    });
    expect(
      screen.queryByText("/private/workspace/that/should/not/render"),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "继续" })).toBeDisabled();
  });

  it("shows the detailed failure reason for failed sessions", () => {
    const props = handlers();
    const failedSession: CodexSessionSummary = {
      ...longSession,
      relay_status: "failed",
      summary: null,
      last_error: "退出码 1；stderr: pnpm test failed",
    };

    render(
      <Collaboration
        bots={[bot]}
        bindings={[longBinding]}
        sessions={[failedSession]}
        contexts={[]}
        profiles={[profile]}
        gatewayModelOptions={[]}
        busy={false}
        {...props}
      />,
    );

    expect(screen.getByText("暂无摘要")).toBeInTheDocument();
    expect(
      screen.getByText("失败原因：退出码 1；stderr: pnpm test failed"),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /取消/ })).not.toBeInTheDocument();
  });
});
