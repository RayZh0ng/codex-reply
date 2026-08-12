import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { MetricsSnapshot } from "../../shared/ipc";
import { Dashboard } from "./Dashboard";

const dialog = vi.hoisted(() => ({ open: vi.fn() }));

vi.mock("@tauri-apps/plugin-dialog", () => dialog);
class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

vi.stubGlobal("ResizeObserver", ResizeObserver);

afterEach(() => cleanup());

const snapshot = {
  workspace_mode: "shared" as const,
  collaboration: {
    enabled_bots: 0,
    bound_chats: 0,
    active_sessions: 0,
  },
  gateway: {
    running: false,
    bind_mode: "loopback" as const,
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
      id: "oauth-profile",
      alias: "工作账号",
      kind: "codex_oauth" as const,
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
      validation_status: "unknown" as const,
      validated_at_ms: null,
      validation_message: null,
    },
  ],
};

function renderDashboard(
  phase: "idle" | "running" = "idle",
  metrics: MetricsSnapshot = snapshot.metrics,
) {
  const onStartTask = vi.fn().mockResolvedValue(undefined);
  const onRequestCancelTask = vi.fn();
  render(
    <Dashboard
      snapshot={{ ...snapshot, metrics }}
      taskStatus={{
        phase,
        profile_id: phase === "running" ? "oauth-profile" : null,
        message:
          phase === "running"
            ? "正在分析工作区并执行任务。"
            : "当前没有正在运行的受管 Codex 任务。",
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
  it("presents the managed task composer as the primary workbench", () => {
    renderDashboard();

    expect(
      screen.getByRole("heading", { name: "受管 Codex 任务" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("任务说明")).toBeInTheDocument();
    expect(screen.queryByText("请求趋势")).not.toBeInTheDocument();
  });

  it("starts one managed task only after a folder and instruction are supplied", async () => {
    dialog.open.mockResolvedValueOnce("/private/workspace");
    const { onStartTask } = renderDashboard();

    const start = screen.getByRole("button", { name: "启动受管任务" });
    expect(start).toBeDisabled();

    fireEvent.change(screen.getByLabelText("任务说明"), {
      target: { value: "检查当前改动" },
    });
    fireEvent.click(screen.getByRole("button", { name: "选择目录" }));

    await waitFor(() =>
      expect(screen.getByText("/private/workspace")).toBeInTheDocument(),
    );
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

    expect(screen.getByText("正在分析工作区并执行任务。")).toBeInTheDocument();
    expect(screen.getAllByText("工作账号")).toHaveLength(2);
    fireEvent.click(screen.getByRole("button", { name: "停止任务" }));

    expect(onRequestCancelTask).toHaveBeenCalledOnce();
    expect(screen.queryByLabelText("任务说明")).not.toBeInTheDocument();
  });

  it("shows rolling gateway latency, throughput, and concurrency metrics", () => {
    renderDashboard("idle", {
      ...snapshot.metrics,
      window_minutes: 60,
      window_requests: 120,
      window_success_rate: 0.975,
      requests_per_minute: 2,
      latency_p50_ms: 80,
      latency_p95_ms: 160,
      latency_p99_ms: 220,
      ttfb_p50_ms: 20,
      ttfb_p95_ms: 45,
      ttfb_p99_ms: 70,
      request_bytes: 2048,
      response_bytes: 4096,
      retry_count: 3,
      active_requests: 4,
      queued_requests: 2,
      telemetry_dropped: 0,
    });

    expect(screen.getByText("近 60 分钟请求")).toBeInTheDocument();
    expect(screen.getByText(/成功率 97\.5% · 2\.00 req\/min/)).toBeInTheDocument();
    expect(screen.getByText("总延迟 P50 / P95 / P99")).toBeInTheDocument();
    expect(screen.getByText("80 ms")).toBeInTheDocument();
    expect(screen.getByText(/160 ms · 220 ms/)).toBeInTheDocument();
    expect(screen.getByText("4 / 2")).toBeInTheDocument();
  });
});
