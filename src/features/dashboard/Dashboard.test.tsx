import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Dashboard } from "./Dashboard";

const dialog = vi.hoisted(() => ({ open: vi.fn() }));

vi.mock("@tauri-apps/plugin-dialog", () => dialog);
vi.mock("recharts", () => ({
  Bar: () => null,
  BarChart: () => null,
  ResponsiveContainer: () => null,
  Tooltip: () => null,
  XAxis: () => null,
}));

class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

vi.stubGlobal("ResizeObserver", ResizeObserver);

afterEach(() => cleanup());

const snapshot = {
  gateway: {
    running: false,
    bind_mode: "loopback" as const,
    bind_address: "127.0.0.1",
    port: 53765,
    cidrs: [],
    available_profiles: 0,
    cooling_profiles: 0,
    client_key_count: 0,
    certificate_ready: false,
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
      id: "oauth-profile",
      alias: "工作账号",
      kind: "codex_oauth" as const,
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
    },
  ],
  notifications: [],
};

function renderDashboard(phase: "idle" | "running" = "idle") {
  const onStartTask = vi.fn().mockResolvedValue(undefined);
  const onRequestCancelTask = vi.fn();
  render(
    <Dashboard
      snapshot={snapshot}
      taskStatus={{
        phase,
        profile_id: phase === "running" ? "oauth-profile" : null,
        message: "当前没有正在运行的受管 Codex 任务。",
      }}
      busy={false}
      onNavigate={vi.fn()}
      onStartTask={onStartTask}
      onRequestCancelTask={onRequestCancelTask}
    />,
  );
  return { onStartTask, onRequestCancelTask };
}

describe("Dashboard managed Codex task", () => {
  it("starts one managed task only after a folder and instruction are supplied", async () => {
    dialog.open.mockResolvedValueOnce("/private/workspace");
    const { onStartTask } = renderDashboard();

    const start = screen.getByRole("button", { name: "启动受管任务" });
    expect(start).toBeDisabled();

    fireEvent.change(screen.getByLabelText("任务说明"), {
      target: { value: "检查当前改动" },
    });
    fireEvent.click(screen.getByRole("button", { name: "选择目录" }));

    await waitFor(() => expect(screen.getByText("已选择工作目录")).toBeInTheDocument());
    fireEvent.click(start);

    expect(onStartTask).toHaveBeenCalledWith({
      instruction: "检查当前改动",
      working_directory: "/private/workspace",
    });
    expect(screen.getByLabelText("任务说明")).toHaveValue("");
    expect(screen.queryByText("/private/workspace")).not.toBeInTheDocument();
  });

  it("shows the active managed task and requests confirmation before stopping it", () => {
    const { onRequestCancelTask } = renderDashboard("running");

    expect(screen.getByText("正在使用“工作账号”运行受管任务。")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "停止任务" }));

    expect(onRequestCancelTask).toHaveBeenCalledOnce();
    expect(screen.queryByLabelText("任务说明")).not.toBeInTheDocument();
  });
});
