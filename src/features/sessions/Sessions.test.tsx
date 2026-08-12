import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { api } from "../../shared/ipc";
import { Sessions } from "./Sessions";

const dialog = vi.hoisted(() => ({
  open: vi.fn(),
  save: vi.fn(),
}));
const events = vi.hoisted(() => ({ listen: vi.fn() }));

vi.mock("@tauri-apps/plugin-dialog", () => dialog);
vi.mock("@tauri-apps/api/event", () => events);

vi.mock("../../shared/ipc", () => ({
  CODEX_HISTORY_SYNC_FINISHED_EVENT: "codex-history-sync-finished",
  api: {
    deleteCodexHistory: vi.fn(),
    exportCodexHistory: vi.fn(),
    importCodexHistory: vi.fn(),
    listCodexHistory: vi.fn(),
    syncCodexHistory: vi.fn(),
  },
}));

let eventHandlers: Record<string, (event: { payload: unknown }) => void> = {};

const report = {
  scanned_at_ms: 1785369600000,
  limit: 100,
  offset: 0,
  total_sessions: 1,
  selected_project_id: "project:project",
  projects: [
    {
      id: "project:project",
      name: "project",
      cwd: "/Users/test/project",
      session_count: 1,
      consistent_count: 0,
      missing_count: 1,
      conflict_count: 0,
      needs_repair_count: 0,
      updated_at_ms: 1785369600000,
    },
  ],
  homes: [
    {
      id: "default",
      kind: "default",
      label: "默认 Codex",
      path: "/Users/test/.codex",
      sync_target: true,
      session_count: 1,
    },
    {
      id: "collaboration_session:turn",
      kind: "collaboration_session",
      label: "协作会话：turn",
      path: "/Users/test/Library/Application Support/codex-relay/collaboration",
      sync_target: false,
      session_count: 1,
    },
  ],
  sessions: [
    {
      id: "019fb252-57f9-7d43-b564-569ff39094a0",
      title: "账号切换历史一致性",
      cwd: "/Users/test/project",
      project_id: "project:project",
      project_name: "project",
      updated_at_ms: 1785369600000,
      status: "missing",
      source_count: 1,
      missing_target_count: 1,
      divergent_source_count: 0,
      sources: [
        {
          home_id: "default",
          home_label: "默认 Codex",
          home_kind: "default",
          rollout_path: "/Users/test/.codex/sessions/rollout.jsonl",
          archived: false,
          updated_at_ms: 1785369600000,
          event_count: 2,
          sha256: "abc123",
        },
      ],
    },
  ],
  warnings: [],
};

beforeEach(() => {
  eventHandlers = {};
  events.listen.mockImplementation(
    (event: string, handler: (event: { payload: unknown }) => void) => {
      eventHandlers[event] = handler;
      return Promise.resolve(vi.fn());
    },
  );
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  dialog.open.mockReset();
  dialog.save.mockReset();
  events.listen.mockReset();
});

describe("Sessions", () => {
  it("renders codex history sessions and homes", async () => {
    vi.mocked(api.listCodexHistory).mockResolvedValue(report);

    render(<Sessions busy={false} onNotice={vi.fn()} />);

    expect(await screen.findByText("账号切换历史一致性")).toBeInTheDocument();
    expect(api.listCodexHistory).toHaveBeenCalledWith({
      limit: 100,
      offset: 0,
      project_id: null,
    });
    expect(screen.getByRole("button", { name: /project/ })).toBeInTheDocument();
    expect(screen.getByText("缺失")).toBeInTheDocument();
    expect(screen.getAllByText("默认 Codex").length).toBeGreaterThan(0);
    expect(screen.getByText("同步目标")).toBeInTheDocument();
    expect(screen.getByText("仅作来源")).toBeInTheDocument();
  });

  it("syncs and reloads history", async () => {
    const onNotice = vi.fn();
    vi.mocked(api.listCodexHistory).mockResolvedValue(report);
    vi.mocked(api.syncCodexHistory).mockResolvedValue({
      status: "completed",
      message: "Codex 会话历史已同步。",
      scanned_at_ms: 1785369601000,
      homes_scanned: 2,
      sessions_seen: 1,
      sessions_synced: 1,
      files_written: 2,
      files_backed_up: 1,
      metadata_rebuilt: 1,
      warnings: [],
    });

    render(<Sessions busy={false} onNotice={onNotice} />);
    fireEvent.click(await screen.findByRole("button", { name: /同步\/修复/ }));

    await waitFor(() => expect(api.syncCodexHistory).toHaveBeenCalledOnce());
    expect(onNotice).toHaveBeenCalledWith("Codex 会话历史已同步。");
    await waitFor(() => expect(api.listCodexHistory).toHaveBeenCalledTimes(2));
  });

  it("refreshes the current project after background transition sync completes", async () => {
    vi.mocked(api.listCodexHistory).mockResolvedValue(report);

    render(<Sessions busy={false} onNotice={vi.fn()} />);
    await screen.findByText("账号切换历史一致性");
    await waitFor(() =>
      expect(events.listen).toHaveBeenCalledWith(
        "codex-history-sync-finished",
        expect.any(Function),
      ),
    );

    emitTauriEvent("codex-history-sync-finished", {
      status: "completed",
      message: "Codex 会话历史已在后台恢复。",
      queued_at_ms: 1785369601000,
      completed_at_ms: 1785369602000,
      warnings: [],
    });

    expect(await screen.findByText("Codex 会话历史已在后台恢复。")).toBeInTheDocument();
    await waitFor(() => expect(api.listCodexHistory).toHaveBeenCalledTimes(2));
    expect(api.listCodexHistory).toHaveBeenLastCalledWith({
      limit: 100,
      offset: 0,
      project_id: "project:project",
    });
  });

  it("imports and exports history through file dialogs", async () => {
    const onNotice = vi.fn();
    vi.mocked(api.listCodexHistory).mockResolvedValue(report);
    vi.mocked(api.importCodexHistory).mockResolvedValue({
      status: "completed",
      message: "已导入 1 个 Codex 会话。",
      scanned_at_ms: 1785369601000,
      sessions_imported: 1,
      files_written: 2,
      files_backed_up: 1,
      metadata_rebuilt: 1,
      warnings: [],
    });
    vi.mocked(api.exportCodexHistory).mockResolvedValue({
      status: "completed",
      message: "已导出 1 个 Codex 会话。",
      scanned_at_ms: 1785369601000,
      sessions_exported: 1,
      files_exported: 2,
      destination_path: "/tmp/history.zip",
      warnings: [],
    });
    dialog.open.mockResolvedValueOnce(["/tmp/history.codex-history.zip"]);
    dialog.save.mockResolvedValueOnce("/tmp/history.codex-history.zip");

    render(<Sessions busy={false} onNotice={onNotice} />);
    fireEvent.click(await screen.findByRole("button", { name: "导入" }));
    await waitFor(() =>
      expect(api.importCodexHistory).toHaveBeenCalledWith([
        "/tmp/history.codex-history.zip",
      ]),
    );
    expect(onNotice).toHaveBeenCalledWith("已导入 1 个 Codex 会话。");

    fireEvent.click(screen.getByRole("button", { name: "导出项目" }));
    await waitFor(() =>
      expect(api.exportCodexHistory).toHaveBeenCalledWith({
        scope: "project",
        session_ids: [],
        project_id: "project:project",
        destination_path: "/tmp/history.codex-history.zip",
      }),
    );
  });

  it("deletes selected sessions after confirmation", async () => {
    const onNotice = vi.fn();
    vi.mocked(api.listCodexHistory).mockResolvedValue(report);
    vi.mocked(api.deleteCodexHistory).mockResolvedValue({
      status: "completed",
      message: "已将 1 个 Codex 会话移入 Relay 回收站。",
      scanned_at_ms: 1785369601000,
      sessions_affected: 1,
      files_removed: 1,
      files_backed_up: 2,
      metadata_updated: 1,
      metadata_rebuilt: 1,
      warnings: [],
    });

    render(<Sessions busy={false} onNotice={onNotice} />);
    fireEvent.click(await screen.findByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: /删除选中/ }));
    fireEvent.click(await screen.findByRole("button", { name: "移入回收站" }));

    await waitFor(() =>
      expect(api.deleteCodexHistory).toHaveBeenCalledWith({
        scope: "sessions",
        session_ids: ["019fb252-57f9-7d43-b564-569ff39094a0"],
        project_id: null,
      }),
    );
    expect(onNotice).toHaveBeenCalledWith("已将 1 个 Codex 会话移入 Relay 回收站。");
  });
});

function emitTauriEvent(event: string, payload: unknown) {
  const handler = eventHandlers[event];
  if (!handler) throw new Error(`No Tauri listener registered for ${event}`);
  handler({ payload });
}
