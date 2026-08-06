import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  DashboardSnapshot,
  GatewayCodexConfigStatus,
  MaskedProfile,
} from "../shared/ipc";
import App from "./App";

const native = vi.hoisted(() => ({ invoke: vi.fn() }));
const events = vi.hoisted(() => ({ listen: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => native);
vi.mock("@tauri-apps/api/event", () => events);

class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

vi.stubGlobal("ResizeObserver", ResizeObserver);

let eventHandlers: Record<string, (event: { payload: unknown }) => void> = {};

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  eventHandlers = {};
  events.listen.mockImplementation(
    (event: string, handler: (event: { payload: unknown }) => void) => {
      eventHandlers[event] = handler;
      return Promise.resolve(vi.fn());
    },
  );
});

const snapshot: DashboardSnapshot = {
  workspace_mode: "shared" as const,
  collaboration: {
    enabled_bots: 0,
    bound_chats: 0,
    active_sessions: 0,
  },
  gateway: {
    running: false,
    bind_mode: "loopback",
    bind_address: "127.0.0.1",
    available_addresses: [],
    port: 53765,
    cidrs: [],
    available_profiles: 0,
    cooling_profiles: 0,
    client_key_count: 0,
    certificate_ready: false,
    service_url: "https://127.0.0.1:53765",
    upstream_proxy_mode: "system" as const,
    upstream_proxy_display: null,
    upstream_last_error: null,
  },
  metrics: {
    total_requests: 0,
    successful_requests: 0,
    failed_requests: 0,
    average_latency_ms: null,
    estimated_tokens: 0,
  },
  profiles: [
    {
      id: "current",
      alias: "当前账号",
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
      is_current: true,
      validation_status: "unknown",
      validated_at_ms: null,
      validation_message: null,
    },
    {
      id: "next",
      alias: "目标账号",
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
      is_current: false,
      validation_status: "unknown",
      validated_at_ms: null,
      validation_message: null,
    },
  ],
};

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  vi.useRealTimers();
  native.invoke.mockReset();
  events.listen.mockReset();
  vi.restoreAllMocks();
  delete (window as typeof window & { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
});

describe("App", () => {
  it("renders the secure native-runtime loading state", () => {
    render(<App />);

    expect(
      screen.getByRole("heading", { name: "正在连接安全本机核心…" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Codex Relay")).toBeInTheDocument();
  });

  it("uses collaboration as the fourth tab and hides the old notifications entry", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "list_collaboration_bots") return Promise.resolve([]);
      if (command === "list_collaboration_project_bindings") return Promise.resolve([]);
      if (command === "list_codex_sessions") return Promise.resolve([]);
      if (command === "list_collaboration_contexts") return Promise.resolve([]);
      if (command === "list_gateway_model_options") return Promise.resolve([]);
      if (command === "collaboration_callback_status")
        return Promise.resolve({
          local_url: "http://127.0.0.1:35817",
          public_urls: [],
          running: false,
        });
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);

    const collaboration = await screen.findByRole("button", { name: "协作" });
    expect(screen.queryByRole("button", { name: "通知" })).not.toBeInTheDocument();
    fireEvent.click(collaboration);

    expect(
      await screen.findByRole("heading", { name: "连接群聊" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /QQ/ })).toBeInTheDocument();
    expect(screen.queryByText("即将支持")).not.toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /企业微信/ })).toBeInTheDocument();
  });

  it("checks the selected update channel on startup and prompts before installing", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation(
      (command: string, args?: Record<string, unknown>) => {
        if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
        if (command === "managed_task_status")
          return Promise.resolve(idleTaskStatusForTest());
        if (command === "app_update_settings")
          return Promise.resolve({ channel: "beta", auto_check: true });
        if (command === "check_app_update") {
          expect(args).toEqual({ input: { channel: "beta" } });
          return Promise.resolve({
            version: "0.2.0-beta.1",
            current_version: "0.1.0",
            body: "Beta 更新说明",
            date: null,
            channel: "beta",
          });
        }
        if (command === "install_app_update") {
          expect(args).toEqual({ input: { channel: "beta" } });
          return Promise.resolve();
        }
        return Promise.reject(new Error(`unexpected command: ${command}`));
      },
    );

    render(<App />);

    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("安装 Codex Relay 0.2.0-beta.1");
    fireEvent.click(screen.getByRole("button", { name: "安装并重启" }));

    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("install_app_update", {
        input: { channel: "beta" },
      }),
    );
  });

  it("shows updater progress events in the global dialog and settings page", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "app_update_settings")
        return Promise.resolve({ channel: "beta", auto_check: false });
      if (command === "list_desktop_workspaces") return Promise.resolve([]);
      if (command === "codex_environment_status")
        return Promise.resolve(emptyCodexEnvironmentForTest());
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    await screen.findByRole("heading", { name: "今天，服务一切就绪。" });

    act(() => {
      emitTauriEvent("app-update-progress", {
        phase: "downloading",
        channel: "beta",
        version: "0.2.0-beta.2",
        current_version: "0.2.0-beta.1",
        downloaded_bytes: 2048,
        content_length: 5120,
        progress_percent: 40,
        message: "正在下载更新。",
        updated_at_ms: 1_700_000_000_000,
      });
    });

    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "正在更新到 Codex Relay 0.2.0-beta.2",
    );
    expect(screen.getByRole("dialog")).toHaveTextContent("40%");

    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    expect(
      await screen.findByRole("heading", { name: "软件更新" }),
    ).toBeInTheDocument();
    expect(screen.getAllByText("40%").length).toBeGreaterThanOrEqual(2);

    act(() => {
      emitTauriEvent("app-update-progress", {
        phase: "failed",
        channel: "beta",
        version: "0.2.0-beta.2",
        current_version: "0.2.0-beta.1",
        downloaded_bytes: 2048,
        content_length: 5120,
        progress_percent: 40,
        message: "更新下载或安装失败，请稍后重试。",
        updated_at_ms: 1_700_000_001_000,
      });
    });

    expect(screen.getByRole("dialog")).toHaveTextContent("更新下载或安装失败");
    expect(screen.getByRole("button", { name: "关闭" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "立即检查更新" })).toBeEnabled();
  });

  it("refreshes app and collaboration state after collaboration operations", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const bot = {
      id: "bot-1",
      provider: "feishu" as const,
      name: "Relay Bot",
      enabled: true,
      connection_status: "connected",
      credential_mask: "cli_••••test",
      config_summary: "App ID cli_••••test",
      callback_public_url: null,
      last_error: null,
      system_prompt: null,
      updated_at_ms: 1,
    };
    const binding = {
      id: "binding-1",
      provider: "feishu" as const,
      bot_id: "bot-1",
      bot_name: "Relay Bot",
      project_name: "Relay",
      project_slug: "relay",
      working_directory: "/tmp",
      profile_id: "current",
      profile_alias: "当前账号",
      chat_id: "chat-1",
      bind_code: "ABCD12",
      enabled: true,
      concurrency_limit: 2,
      execution_target: "profile" as const,
      model_id: null,
      created_at_ms: 1,
      updated_at_ms: 1,
    };
    const session = {
      id: "12345678-session",
      binding_id: "binding-1",
      context_id: "ctx-12345678",
      provider: "feishu" as const,
      provider_bot_id: "bot-1",
      provider_chat_id: "chat-1",
      provider_message_id: null,
      project_name: "Relay",
      project_slug: "relay",
      profile_id: "current",
      profile_alias: "当前账号",
      relay_status: "running",
      codex_session_id: "codex-session-1",
      feishu_message_id: null,
      feishu_chat_id: "chat-1",
      execution_target: "profile" as const,
      model_id: null,
      started_by: "sender-1",
      started_at_ms: 1,
      updated_at_ms: 1,
      finished_at_ms: null,
      summary: "任务运行中。",
      last_error: null,
      turn_kind: "natural",
      conversation_mode: "default",
      goal_status: null,
    };
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "list_collaboration_bots") return Promise.resolve([bot]);
      if (command === "list_collaboration_project_bindings")
        return Promise.resolve([binding]);
      if (command === "list_codex_sessions") return Promise.resolve([session]);
      if (command === "list_collaboration_contexts") return Promise.resolve([]);
      if (command === "list_gateway_model_options") return Promise.resolve(["gpt-5"]);
      if (command === "collaboration_callback_status")
        return Promise.resolve({
          local_url: "http://127.0.0.1:35817",
          public_urls: [],
          running: false,
        });
      if (command === "test_collaboration_bot") return Promise.resolve(bot);
      if (command === "continue_codex_session") return Promise.resolve(session);
      if (command === "cancel_codex_session")
        return Promise.resolve({ ...session, relay_status: "cancelled" });
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "协作" }));
    await screen.findByRole("heading", { name: "Relay Bot" });
    await waitFor(() => expect(commandCalls("list_codex_sessions")).toHaveLength(1));

    fireEvent.click(screen.getByRole("button", { name: /测试/ }));
    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("test_collaboration_bot", {
        id: "bot-1",
      }),
    );
    await waitFor(() => {
      expect(commandCalls("dashboard_snapshot").length).toBeGreaterThanOrEqual(2);
      expect(commandCalls("list_codex_sessions").length).toBeGreaterThanOrEqual(2);
      expect(commandCalls("list_gateway_model_options").length).toBeGreaterThanOrEqual(
        2,
      );
    });

    fireEvent.change(screen.getByPlaceholderText("追加说明后继续会话"), {
      target: { value: "继续处理" },
    });
    fireEvent.click(screen.getByRole("button", { name: "继续" }));
    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("continue_codex_session", {
        input: {
          session_id: "12345678-session",
          instruction: "继续处理",
          confirmed: true,
        },
      }),
    );
    await waitFor(() =>
      expect(commandCalls("list_codex_sessions").length).toBeGreaterThanOrEqual(3),
    );

    fireEvent.click(screen.getByRole("button", { name: /取消/ }));
    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("cancel_codex_session", {
        input: { session_id: "12345678-session", confirmed: true },
      }),
    );
    await waitFor(() =>
      expect(commandCalls("list_codex_sessions").length).toBeGreaterThanOrEqual(4),
    );
  });

  it("loads only first-screen data before a page is opened", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    const { container } = render(<App />);
    await screen.findByRole("heading", { name: "今天，服务一切就绪。" });

    expect(native.invoke).not.toHaveBeenCalledWith(
      "list_collaboration_bots",
      undefined,
    );
    expect(native.invoke).not.toHaveBeenCalledWith(
      "list_desktop_workspaces",
      undefined,
    );

    const scrollContainer = container.querySelector(".content-scroll");
    expect(scrollContainer).not.toBeNull();
    Object.defineProperty(scrollContainer, "scrollTop", {
      configurable: true,
      value: 12,
    });
    fireEvent.scroll(scrollContainer as Element);
    expect(container.querySelector(".topbar")).toHaveClass("is-scrolled");
  });

  it("persists the wide sidebar rail preference", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });
    const { container } = render(<App />);
    await screen.findByRole("heading", { name: "今天，服务一切就绪。" });

    fireEvent.click(screen.getByRole("button", { name: "收起侧边栏" }));
    expect(container.querySelector(".app-shell")).toHaveClass("is-sidebar-collapsed");
    expect(window.localStorage.getItem("codex-relay.sidebar.v1")).toBe("collapsed");
    expect(screen.getByRole("button", { name: "展开侧边栏" })).toHaveAttribute(
      "aria-expanded",
      "false",
    );
  });

  it("opens the compact sidebar as a dismissible drawer", async () => {
    vi.spyOn(window, "matchMedia").mockImplementation(
      (query) =>
        ({
          matches: query.includes("max-width: 1179px"),
          media: query,
          onchange: null,
          addEventListener: () => undefined,
          removeEventListener: () => undefined,
          addListener: () => undefined,
          removeListener: () => undefined,
          dispatchEvent: () => false,
        }) as MediaQueryList,
    );
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });
    const { container } = render(<App />);
    await screen.findByRole("heading", { name: "今天，服务一切就绪。" });

    expect(container.querySelectorAll(".nav-icon").length).toBeGreaterThan(0);

    const toggle = screen.getByRole("button", { name: "展开侧边栏" });
    fireEvent.click(toggle);
    expect(container.querySelector(".app-shell")).toHaveClass("is-sidebar-drawer-open");
    fireEvent.keyDown(window, { key: "Escape" });
    expect(container.querySelector(".app-shell")).not.toHaveClass(
      "is-sidebar-drawer-open",
    );
    expect(toggle).toHaveFocus();
  });

  it("finishes switching after the desktop restart is requested", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation(
      (command: string, args?: Record<string, unknown>) => {
        if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
        if (command === "managed_task_status") {
          return Promise.resolve({
            phase: "running",
            profile_id: "current",
            message: "受管 Codex 任务正在运行。",
          });
        }
        if (
          command === "select_current_profile" &&
          (args?.input as { id?: string } | undefined)?.id === "next"
        ) {
          return Promise.resolve({
            profile_id: "next",
            attempt_id: "switch-1",
            status: "switching",
            message: "正在写入已保存的 Codex 凭据，并复用原 Codex 客户端状态。",
          });
        }
        if (command === "current_profile_activation_status") {
          return Promise.resolve({
            profile_id: "next",
            attempt_id: "switch-1",
            status: "activated",
            message: "Codex 凭据已切换，并已复用原 Codex 客户端状态。",
          });
        }
        return Promise.reject(new Error(`unexpected command: ${command}`));
      },
    );

    render(<App />);
    await screen.findByRole("button", { name: "档案" });
    fireEvent.click(screen.getByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "设为当前档案：目标账号" }),
    );
    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "关闭并切换 Codex 客户端？",
    );
    fireEvent.click(screen.getByRole("button", { name: "关闭并切换" }));

    await waitFor(() =>
      expect(screen.getByRole("status")).toHaveTextContent(
        "Codex 凭据已切换，并已复用原 Codex 客户端状态。",
      ),
    );
    expect(native.invoke).toHaveBeenCalledWith("select_current_profile", {
      input: { id: "next", confirmed_desktop_restart: true },
    });
  });

  it("does not expose a cancellation control while a credential switch is running", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve({
          phase: "idle",
          profile_id: null,
          message: "当前没有正在运行的受管 Codex 任务。",
        });
      if (command === "select_current_profile") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在写入已保存的 Codex 凭据，并复用原 Codex 客户端状态。",
        });
      }
      if (command === "current_profile_activation_status") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在写入已保存的 Codex 凭据，并复用原 Codex 客户端状态。",
        });
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "设为当前档案：目标账号" }),
    );
    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "关闭并切换 Codex 客户端？",
    );
    fireEvent.click(screen.getByRole("button", { name: "关闭并切换" }));
    expect(await screen.findByRole("dialog")).toHaveTextContent("正在切换已保存的账号");
    expect(screen.queryByRole("button", { name: "取消登录" })).not.toBeInTheDocument();
  });

  it("requires confirmation before restarting the shared desktop client", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const sharedSnapshot = { ...snapshot, workspace_mode: "shared" as const };
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(sharedSnapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "select_current_profile") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在切换。",
        });
      }
      if (command === "current_profile_activation_status") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在切换。",
        });
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "设为当前档案：目标账号" }),
    );
    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "关闭并切换 Codex 客户端？",
    );
    expect(native.invoke).not.toHaveBeenCalledWith(
      "select_current_profile",
      expect.anything(),
    );

    fireEvent.click(screen.getByRole("button", { name: "关闭并切换" }));
    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("select_current_profile", {
        input: { id: "next", confirmed_desktop_restart: true },
      }),
    );
  });

  it("reports an auth file failure without suggesting the generic ChatGPT login flow", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status") {
        return Promise.resolve({
          phase: "idle",
          profile_id: null,
          message: "当前没有正在运行的受管 Codex 任务。",
        });
      }
      if (command === "select_current_profile") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在切换。",
        });
      }
      if (command === "current_profile_activation_status") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "auth_file_write_failed",
          message: "无法更新默认 Codex auth.json；请确认文件权限后重试。",
        });
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "设为当前档案：目标账号" }),
    );
    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "关闭并切换 Codex 客户端？",
    );
    fireEvent.click(screen.getByRole("button", { name: "关闭并切换" }));
    await waitFor(() =>
      expect(screen.getByRole("status")).toHaveTextContent(
        "无法更新默认 Codex auth.json；请确认文件权限后重试",
      ),
    );
  });

  it("unblocks the interface when activation polling exceeds its deadline", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status") {
        return Promise.resolve({
          phase: "idle",
          profile_id: null,
          message: "当前没有正在运行的受管 Codex 任务。",
        });
      }
      if (command === "select_current_profile") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在切换。",
        });
      }
      if (command === "current_profile_activation_status") {
        return Promise.resolve({
          profile_id: "next",
          attempt_id: "switch-1",
          status: "switching",
          message: "正在切换。",
        });
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    const target = await screen.findByRole("button", {
      name: "设为当前档案：目标账号",
    });
    fireEvent.click(target);
    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "关闭并切换 Codex 客户端？",
    );
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "关闭并切换" }));
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByRole("dialog")).toHaveTextContent("正在切换已保存的账号");
    await act(async () => {
      vi.advanceTimersByTime(20_000);
      await Promise.resolve();
    });

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("账号切换状态超时");
  });

  it("refreshes Codex account summaries on the visible profile page every 30 seconds without overlap", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    let finishFirstRefresh: (() => void) | undefined;
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "desktop_workspace_settings")
        return Promise.resolve({ mode: "per_profile" });
      if (command === "list_desktop_workspaces") return Promise.resolve([]);
      if (command === "refresh_profile_quotas") {
        return new Promise((resolve) => {
          finishFirstRefresh = () => resolve({ profiles: [], failed_profile_ids: [] });
        });
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    await screen.findByRole("button", { name: "档案" });
    fireEvent.click(screen.getByRole("button", { name: "档案" }));
    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("refresh_profile_quotas", undefined),
    );

    vi.useFakeTimers();
    await act(async () => {
      window.dispatchEvent(new Event("focus"));
      vi.advanceTimersByTime(30_000);
      await Promise.resolve();
    });
    expect(
      native.invoke.mock.calls.filter(
        ([command]) => command === "refresh_profile_quotas",
      ),
    ).toHaveLength(1);

    await act(async () => {
      finishFirstRefresh?.();
      await Promise.all(Array.from({ length: 8 }, () => Promise.resolve()));
    });
    vi.useRealTimers();
    window.dispatchEvent(new Event("focus"));
    await waitFor(() =>
      expect(
        native.invoke.mock.calls.filter(
          ([command]) => command === "refresh_profile_quotas",
        ),
      ).toHaveLength(2),
    );
  });

  it("refreshes missing models before adding a profile to the gateway pool", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation(
      (command: string, args?: Record<string, unknown>) => {
        if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
        if (command === "managed_task_status")
          return Promise.resolve(idleTaskStatusForTest());
        if (command === "desktop_workspace_settings")
          return Promise.resolve({ mode: "per_profile" });
        if (command === "list_desktop_workspaces") return Promise.resolve([]);
        if (command === "refresh_profile_quotas")
          return Promise.resolve({ profiles: [], failed_profile_ids: [] });
        if (command === "refresh_profile_models") {
          expect(args).toEqual({ id: "current" });
          return Promise.resolve({
            ...snapshot.profiles[0],
            models: ["gpt-5"],
          });
        }
        if (command === "update_profile") {
          expect(args).toEqual({
            input: expect.objectContaining({
              id: "current",
              in_pool: true,
              models: ["gpt-5"],
            }),
          });
          return Promise.resolve({
            ...snapshot.profiles[0],
            in_pool: true,
            models: ["gpt-5"],
          });
        }
        return Promise.reject(new Error(`unexpected command: ${command}`));
      },
    );

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "加入网关账号池：当前账号" }),
    );

    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("update_profile", {
        input: expect.objectContaining({
          id: "current",
          in_pool: true,
          models: ["gpt-5"],
        }),
      }),
    );
  });

  it("updates active direct API profile OAuth binding through the sync command", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const oauthA = {
      ...snapshot.profiles[0],
      id: "oauth-a",
      alias: "Work A",
      is_current: true,
    };
    const oauthB = {
      ...snapshot.profiles[1],
      id: "oauth-b",
      alias: "Work B",
      is_current: false,
    };
    const apiProfile = apiProfileForAppTest("api-active", "oauth-a");
    let currentSnapshot: DashboardSnapshot = {
      ...snapshot,
      profiles: [oauthA, oauthB, apiProfile],
    };
    const activeStatus = gatewayConfigForAppTest({
      enabled: true,
      directProfileId: "api-active",
      oauthProfileId: "oauth-a",
      message: "Codex 正在直连第三方模型提供商：Third Party。",
    });
    native.invoke.mockImplementation(
      (command: string, args?: Record<string, unknown>) => {
        if (command === "dashboard_snapshot") return Promise.resolve(currentSnapshot);
        if (command === "managed_task_status")
          return Promise.resolve(idleTaskStatusForTest());
        if (command === "refresh_profile_quotas")
          return Promise.resolve({ profiles: [], failed_profile_ids: [] });
        if (command === "codex_gateway_config_status")
          return Promise.resolve(activeStatus);
        if (command === "update_api_service_profile") {
          expect(args).toEqual({
            input: expect.objectContaining({
              id: "api-active",
              codex_oauth_profile_id: "oauth-b",
              provider: "openai_compatible",
              wire_api: "responses",
              base_url: "https://api.example.test/v1",
            }),
          });
          const updatedProfile = { ...apiProfile, codex_oauth_profile_id: "oauth-b" };
          currentSnapshot = {
            ...currentSnapshot,
            profiles: [oauthA, oauthB, updatedProfile],
          };
          return Promise.resolve({
            profile: updatedProfile,
            codex_config: {
              ...activeStatus,
              oauth_profile_id: "oauth-b",
              oauth_profile_alias: "Work B",
              message: "已同步 OAuth 登录档案，已重启 Codex。",
            },
          });
        }
        if (command === "list_collaboration_bots") return Promise.resolve([]);
        if (command === "list_collaboration_project_bindings")
          return Promise.resolve([]);
        if (command === "list_codex_sessions") return Promise.resolve([]);
        if (command === "list_collaboration_contexts") return Promise.resolve([]);
        if (command === "list_gateway_model_options")
          return Promise.resolve(["codex-visible"]);
        return Promise.reject(new Error(`unexpected command: ${command}`));
      },
    );

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "编辑 API 服务：Third Party" }),
    );
    const sheet = await screen
      .findByRole("heading", { name: "编辑第三方模型提供商" })
      .then((heading) => heading.closest("section") as HTMLElement);
    const form = within(sheet);
    const oauthSelector = form.getByRole("combobox", {
      name: "OAuth 登录档案（可选）",
    });
    await waitFor(() => expect(oauthSelector).toHaveTextContent("Work A"));
    fireEvent.click(oauthSelector);
    fireEvent.click(await screen.findByRole("option", { name: /Work B/ }));
    fireEvent.click(form.getByRole("button", { name: "保存修改" }));

    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("update_api_service_profile", {
        input: expect.objectContaining({
          id: "api-active",
          codex_oauth_profile_id: "oauth-b",
        }),
      }),
    );
    expect(screen.getByRole("status")).toHaveTextContent(
      "已同步 OAuth 登录档案，已重启 Codex。",
    );
    expect(
      native.invoke.mock.calls.some(
        ([command, args]) =>
          command === "update_profile" &&
          (args as { input?: { id?: string } } | undefined)?.input?.id === "api-active",
      ),
    ).toBe(false);
  });

  it("shows the ordinary provider update notice when an API profile is inactive", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const oauthA = {
      ...snapshot.profiles[0],
      id: "oauth-a",
      alias: "Work A",
      is_current: true,
    };
    const oauthB = {
      ...snapshot.profiles[1],
      id: "oauth-b",
      alias: "Work B",
      is_current: false,
    };
    const apiProfile = apiProfileForAppTest("api-inactive", "oauth-a");
    let currentSnapshot: DashboardSnapshot = {
      ...snapshot,
      profiles: [oauthA, oauthB, apiProfile],
    };
    const inactiveStatus = gatewayConfigForAppTest({
      enabled: false,
      directProfileId: null,
      oauthProfileId: null,
      message: "Codex 正在使用官方模型配置。",
    });
    native.invoke.mockImplementation(
      (command: string, args?: Record<string, unknown>) => {
        if (command === "dashboard_snapshot") return Promise.resolve(currentSnapshot);
        if (command === "managed_task_status")
          return Promise.resolve(idleTaskStatusForTest());
        if (command === "refresh_profile_quotas")
          return Promise.resolve({ profiles: [], failed_profile_ids: [] });
        if (command === "codex_gateway_config_status")
          return Promise.resolve(inactiveStatus);
        if (command === "update_api_service_profile") {
          expect(args).toEqual({
            input: expect.objectContaining({
              id: "api-inactive",
              codex_oauth_profile_id: "oauth-b",
            }),
          });
          const updatedProfile = { ...apiProfile, codex_oauth_profile_id: "oauth-b" };
          currentSnapshot = {
            ...currentSnapshot,
            profiles: [oauthA, oauthB, updatedProfile],
          };
          return Promise.resolve({
            profile: updatedProfile,
            codex_config: null,
          });
        }
        if (command === "list_collaboration_bots") return Promise.resolve([]);
        if (command === "list_collaboration_project_bindings")
          return Promise.resolve([]);
        if (command === "list_codex_sessions") return Promise.resolve([]);
        if (command === "list_collaboration_contexts") return Promise.resolve([]);
        if (command === "list_gateway_model_options")
          return Promise.resolve(["codex-visible"]);
        return Promise.reject(new Error(`unexpected command: ${command}`));
      },
    );

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "档案" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "编辑 API 服务：Third Party" }),
    );
    const sheet = await screen
      .findByRole("heading", { name: "编辑第三方模型提供商" })
      .then((heading) => heading.closest("section") as HTMLElement);
    const form = within(sheet);
    const oauthSelector = form.getByRole("combobox", {
      name: "OAuth 登录档案（可选）",
    });
    await waitFor(() => expect(oauthSelector).toHaveTextContent("Work A"));
    fireEvent.click(oauthSelector);
    fireEvent.click(await screen.findByRole("option", { name: /Work B/ }));
    fireEvent.click(form.getByRole("button", { name: "保存修改" }));

    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("update_api_service_profile", {
        input: expect.objectContaining({
          id: "api-inactive",
          codex_oauth_profile_id: "oauth-b",
        }),
      }),
    );
    expect(screen.getByRole("status")).toHaveTextContent("第三方模型提供商已更新。");
    expect(screen.getByRole("status")).not.toHaveTextContent("重启");
  });

  it("keeps a background Keychain authorization requirement out of the global error state", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(snapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "desktop_workspace_settings")
        return Promise.resolve({ mode: "per_profile" });
      if (command === "list_desktop_workspaces") return Promise.resolve([]);
      if (command === "refresh_profile_quotas") {
        return Promise.reject(new Error("keychain interaction required"));
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    await screen.findByRole("button", { name: "档案" });
    fireEvent.click(screen.getByRole("button", { name: "档案" }));
    await waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith("refresh_profile_quotas", undefined),
    );

    expect(screen.queryByText("keychain interaction required")).not.toBeInTheDocument();
  });

  it("notifies once per valid-to-invalid transition and rearms after recovery", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    let currentSnapshot: DashboardSnapshot = {
      ...snapshot,
      profiles: [
        {
          ...snapshot.profiles[0],
          validation_status: "valid" as const,
          validation_message: "官方 Codex 接口验证通过。",
        },
      ],
    };
    const validationSequence = ["invalid", "invalid", "valid", "invalid"] as const;
    let validationIndex = 0;
    native.invoke.mockImplementation((command: string) => {
      if (command === "dashboard_snapshot") return Promise.resolve(currentSnapshot);
      if (command === "managed_task_status")
        return Promise.resolve(idleTaskStatusForTest());
      if (command === "desktop_workspace_settings")
        return Promise.resolve({ mode: "per_profile" });
      if (command === "list_desktop_workspaces") return Promise.resolve([]);
      if (command === "refresh_profile_quotas") {
        const validationStatus =
          validationSequence[Math.min(validationIndex, validationSequence.length - 1)];
        validationIndex += 1;
        const profile = {
          ...currentSnapshot.profiles[0],
          validation_status: validationStatus,
          validation_message:
            validationStatus === "invalid"
              ? "官方 Codex 接口拒绝了当前登录凭据，请重新授权。"
              : "官方 Codex 接口验证通过。",
        };
        currentSnapshot = { ...currentSnapshot, profiles: [profile] };
        return Promise.resolve({ profiles: [profile], failed_profile_ids: [] });
      }
      return Promise.reject(new Error(`unexpected command: ${command}`));
    });

    render(<App />);
    const profilesButton = await screen.findByRole("button", { name: "档案" });
    vi.useFakeTimers();
    await act(async () => {
      fireEvent.click(profilesButton);
      await Promise.all(Array.from({ length: 8 }, () => Promise.resolve()));
    });
    expect(
      screen.getByText("档案 当前账号 已失效，请重新授权后再使用。"),
    ).toBeInTheDocument();
    await act(async () => {
      vi.advanceTimersByTime(4_000);
      await Promise.resolve();
    });
    expect(
      screen.queryByText("档案 当前账号 已失效，请重新授权后再使用。"),
    ).not.toBeInTheDocument();
    vi.useRealTimers();

    window.dispatchEvent(new Event("focus"));
    await waitFor(() => {
      expect(commandCalls("refresh_profile_quotas")).toHaveLength(2);
      expect(commandCalls("dashboard_snapshot")).toHaveLength(3);
    });
    expect(
      screen.queryByText("档案 当前账号 已失效，请重新授权后再使用。"),
    ).not.toBeInTheDocument();

    window.dispatchEvent(new Event("focus"));
    await waitFor(() => {
      expect(commandCalls("refresh_profile_quotas")).toHaveLength(3);
      expect(commandCalls("dashboard_snapshot")).toHaveLength(4);
      expect(screen.getByLabelText("档案有效性：当前账号")).toHaveTextContent(
        "档案有效",
      );
    });
    window.dispatchEvent(new Event("focus"));
    await waitFor(() => {
      expect(commandCalls("refresh_profile_quotas")).toHaveLength(4);
      expect(commandCalls("dashboard_snapshot")).toHaveLength(5);
    });
    expect(
      await screen.findByText("档案 当前账号 已失效，请重新授权后再使用。"),
    ).toBeInTheDocument();
  });
});

function apiProfileForAppTest(id: string, codexOAuthProfileId: string): MaskedProfile {
  return {
    id,
    alias: "Third Party",
    kind: "api_key",
    base_url: "https://api.example.test/v1",
    provider: "openai_compatible",
    wire_api: "responses",
    enabled: true,
    in_pool: false,
    priority: 0,
    weight: 1,
    models: ["codex-visible"],
    model_mappings: [
      {
        model: "codex-visible",
        upstream_model: "provider-real",
        display_name: "Provider Real",
        context_window: null,
      },
    ],
    codex_oauth_profile_id: codexOAuthProfileId,
    health: "healthy",
    cooldown_until_ms: null,
    credential_configured: true,
    is_current: false,
    validation_status: "unknown",
    validated_at_ms: null,
    validation_message: null,
  };
}

function gatewayConfigForAppTest({
  enabled,
  directProfileId,
  oauthProfileId,
  message,
}: {
  enabled: boolean;
  directProfileId: string | null;
  oauthProfileId: string | null;
  message: string;
}): GatewayCodexConfigStatus {
  return {
    enabled,
    mode: enabled && directProfileId ? "third_party" : "official",
    config_path: "/Users/test/.codex/config.toml",
    service_url: enabled ? "https://api.example.test/v1" : null,
    message,
    auth_status: enabled ? "ok" : "missing",
    needs_repair: false,
    direct_profile_id: directProfileId,
    direct_profile_alias: directProfileId ? "Third Party" : null,
    oauth_profile_id: oauthProfileId,
    oauth_profile_alias:
      oauthProfileId === "oauth-a"
        ? "Work A"
        : oauthProfileId === "oauth-b"
          ? "Work B"
          : null,
    oauth_profile_available: Boolean(oauthProfileId),
    oauth_profile_options: [
      {
        id: "oauth-a",
        alias: "Work A",
        available: true,
        reason: null,
      },
      {
        id: "oauth-b",
        alias: "Work B",
        available: true,
        reason: null,
      },
    ],
    history_sync: null,
    history_sync_status: null,
  };
}

function commandCalls(command: string) {
  return native.invoke.mock.calls.filter(([name]) => name === command);
}

function emitTauriEvent(event: string, payload: unknown) {
  const handler = eventHandlers[event];
  if (!handler) throw new Error(`No Tauri listener registered for ${event}`);
  handler({ payload });
}

function emptyCodexEnvironmentForTest() {
  return {
    platform: "macos",
    codex_home: "/Users/dev/.codex",
    can_install: false,
    message: "Codex 三端最小运行环境检查通过。",
    last_checked_at_ms: 1_700_000_000_000,
    summary: {
      status: "healthy",
      ok_count: 1,
      warning_count: 0,
      missing_count: 0,
      failed_count: 0,
      fixable_count: 0,
      health_percent: 100,
    },
    checks: [],
    install_steps: [],
    manual_commands: [],
  };
}

function idleTaskStatusForTest() {
  return {
    phase: "idle" as const,
    profile_id: null,
    message: "当前没有正在运行的受管 Codex 任务。",
  };
}
