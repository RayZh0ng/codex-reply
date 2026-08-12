import { ArrowsClockwise } from "@phosphor-icons/react/ArrowsClockwise";
import { ClockCounterClockwise } from "@phosphor-icons/react/ClockCounterClockwise";
import { Database } from "@phosphor-icons/react/Database";
import { FolderOpen } from "@phosphor-icons/react/FolderOpen";
import { Trash } from "@phosphor-icons/react/Trash";
import { WarningCircle } from "@phosphor-icons/react/WarningCircle";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  CODEX_HISTORY_SYNC_FINISHED_EVENT,
  api,
  type CodexHistoryProjectSummary,
  type CodexHistoryReport,
  type CodexHistorySessionSummary,
  type CodexHistorySyncReport,
  type CodexHistoryTransitionStatus,
} from "../../shared/ipc";
import {
  Button,
  Dialog,
  EmptyState,
  InlineNotice,
  PageHeader,
  StatusPill,
} from "../../shared/ui";
import "./sessions.css";

interface SessionsProps {
  busy: boolean;
  historySyncStatus?: CodexHistoryTransitionStatus | null;
  onNotice: (message: string) => void;
}

interface DeleteIntent {
  scope: "sessions" | "project";
  sessionIds: string[];
  projectId: string | null;
  label: string;
  count: number;
}

const HISTORY_PAGE_LIMIT = 100;

export function Sessions({ busy, historySyncStatus = null, onNotice }: SessionsProps) {
  const [report, setReport] = useState<CodexHistoryReport | null>(null);
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
  const selectedProjectIdRef = useRef<string | null>(null);
  const [selectedSessionIds, setSelectedSessionIds] = useState<Set<string>>(
    () => new Set(),
  );
  const [syncReport, setSyncReport] = useState<CodexHistorySyncReport | null>(null);
  const [transitionStatus, setTransitionStatus] =
    useState<CodexHistoryTransitionStatus | null>(historySyncStatus);
  const [loading, setLoading] = useState(true);
  const [working, setWorking] = useState(false);
  const [deleteIntent, setDeleteIntent] = useState<DeleteIntent | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async (projectId?: string | null) => {
    setLoading(true);
    try {
      const next = await api.listCodexHistory({
        limit: HISTORY_PAGE_LIMIT,
        offset: 0,
        project_id: projectId ?? null,
      });
      setReport(next);
      setSelectedProjectId(next.selected_project_id);
      setSelectedSessionIds((current) => {
        const visibleIds = new Set(next.sessions.map((session) => session.id));
        return new Set([...current].filter((id) => visibleIds.has(id)));
      });
      setError(null);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Codex 会话历史暂不可读。");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(null);
  }, [load]);

  useEffect(() => {
    selectedProjectIdRef.current = selectedProjectId;
  }, [selectedProjectId]);

  useEffect(() => {
    if (historySyncStatus) setTransitionStatus(historySyncStatus);
  }, [historySyncStatus]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let mounted = true;
    void listen<CodexHistoryTransitionStatus>(
      CODEX_HISTORY_SYNC_FINISHED_EVENT,
      (event) => {
        if (!mounted) return;
        setTransitionStatus(event.payload);
        void load(selectedProjectIdRef.current);
      },
    )
      .then((nextUnlisten) => {
        unlisten = nextUnlisten;
        if (!mounted) unlisten();
      })
      .catch(() => {
        // The event bridge is absent in browser-only unit tests.
      });
    return () => {
      mounted = false;
      unlisten?.();
    };
  }, [load]);

  const selectedProject =
    report?.projects.find((project) => project.id === selectedProjectId) ?? null;
  const selectedCount = selectedSessionIds.size;
  const disabled = busy || loading || working;

  const statusCounts = useMemo(() => {
    const counts = { consistent: 0, missing: 0, conflict: 0, needsRepair: 0 };
    for (const project of report?.projects ?? []) {
      counts.consistent += project.consistent_count;
      counts.missing += project.missing_count;
      counts.conflict += project.conflict_count;
      counts.needsRepair += project.needs_repair_count;
    }
    return counts;
  }, [report?.projects]);

  const selectProject = (projectId: string) => {
    if (disabled || projectId === selectedProjectId) return;
    setSelectedProjectId(projectId);
    setSelectedSessionIds(new Set());
    void load(projectId);
  };

  const sync = async () => {
    setWorking(true);
    try {
      const next = await api.syncCodexHistory();
      setSyncReport(next);
      onNotice(next.message);
      await load(selectedProjectId);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Codex 会话历史同步未完成。");
    } finally {
      setWorking(false);
    }
  };

  const importHistory = async () => {
    setWorking(true);
    try {
      const selected = await open({
        multiple: true,
        title: "导入 Codex 会话历史",
        filters: [
          { name: "Codex history", extensions: ["zip", "jsonl"] },
          { name: "All files", extensions: ["*"] },
        ],
      });
      const paths = Array.isArray(selected) ? selected : selected ? [selected] : [];
      if (!paths.length) return;
      const next = await api.importCodexHistory(paths);
      onNotice(next.message);
      await load(selectedProjectId);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Codex 会话历史导入未完成。");
    } finally {
      setWorking(false);
    }
  };

  const exportHistory = async (scope: "sessions" | "project" | "all") => {
    setWorking(true);
    try {
      const defaultPath =
        scope === "sessions"
          ? "codex-selected.codex-history.zip"
          : scope === "project"
            ? `${sanitizeFileName(selectedProject?.name ?? "codex-project")}.codex-history.zip`
            : "codex-history.codex-history.zip";
      const destination = await save({
        title: "导出 Codex 会话历史",
        defaultPath,
        filters: [{ name: "Codex history", extensions: ["zip"] }],
      });
      if (!destination) return;
      const next = await api.exportCodexHistory({
        scope,
        session_ids: scope === "sessions" ? [...selectedSessionIds] : [],
        project_id: scope === "project" ? selectedProjectId : null,
        destination_path: destination,
      });
      onNotice(next.message);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Codex 会话历史导出未完成。");
    } finally {
      setWorking(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteIntent) return;
    setWorking(true);
    try {
      const next = await api.deleteCodexHistory({
        scope: deleteIntent.scope,
        session_ids: deleteIntent.sessionIds,
        project_id: deleteIntent.projectId,
      });
      setDeleteIntent(null);
      setSelectedSessionIds(new Set());
      onNotice(next.message);
      await load(deleteIntent.scope === "project" ? null : selectedProjectId);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Codex 会话历史删除未完成。");
    } finally {
      setWorking(false);
    }
  };

  const toggleSession = (sessionId: string) => {
    setSelectedSessionIds((current) => {
      const next = new Set(current);
      if (next.has(sessionId)) next.delete(sessionId);
      else next.add(sessionId);
      return next;
    });
  };

  const toggleVisibleSessions = () => {
    setSelectedSessionIds((current) => {
      const visible = report?.sessions.map((session) => session.id) ?? [];
      if (visible.every((id) => current.has(id))) return new Set();
      return new Set(visible);
    });
  };

  const visibleAllSelected =
    !!report?.sessions.length &&
    report.sessions.every((session) => selectedSessionIds.has(session.id));

  return (
    <div className="page sessions-page">
      <PageHeader
        actions={
          <div className="session-page-actions">
            <Button
              disabled={disabled}
              leadingIcon={<ArrowsClockwise size={17} />}
              size="sm"
              variant="quiet"
              onClick={() => void load(selectedProjectId)}
            >
              刷新
            </Button>
            <Button
              disabled={disabled}
              size="sm"
              variant="secondary"
              onClick={() => void importHistory()}
            >
              导入
            </Button>
            <Button
              disabled={disabled}
              size="sm"
              variant="secondary"
              onClick={() => void exportHistory("all")}
            >
              导出全部
            </Button>
            <Button
              disabled={disabled}
              leadingIcon={<ClockCounterClockwise size={17} weight="bold" />}
              loading={working}
              loadingLabel="正在同步"
              size="sm"
              variant="primary"
              onClick={() => void sync()}
            >
              同步/修复
            </Button>
          </div>
        }
        description="按项目管理默认客户端、档案运行时与协作上下文里的 Codex 历史。"
        title="Codex 会话管理"
      />

      {error && (
        <InlineNotice tone="danger" title="会话历史操作未完成">
          {error}
        </InlineNotice>
      )}

      {syncReport && (
        <InlineNotice title={syncReport.message} aria-label="最近一次同步结果">
          <span>
            扫描 {syncReport.homes_scanned} 个 home · 看到 {syncReport.sessions_seen}{" "}
            个会话 · 备份 {syncReport.files_backed_up} 项
          </span>
        </InlineNotice>
      )}

      {transitionStatus && (
        <InlineNotice title={transitionStatus.message} aria-label="后台同步状态">
          <span>
            {transitionStatus.completed_at_ms
              ? `完成于 ${formatDate(transitionStatus.completed_at_ms)}`
              : "账号/网关切换后的历史恢复正在后台执行"}
          </span>
        </InlineNotice>
      )}

      <section className="history-summary-grid" aria-label="Codex 会话历史摘要">
        <HistoryMetric
          label="项目"
          value={loading ? "…" : String(report?.projects.length ?? 0)}
          detail={`${report?.homes.length ?? 0} 个 Codex home`}
        />
        <HistoryMetric
          label="会话"
          value={loading ? "…" : String(report?.total_sessions ?? 0)}
          detail={
            selectedProject
              ? `${selectedProject.name} · 首屏 ${report?.sessions.length ?? 0} 条`
              : "等待扫描"
          }
        />
        <HistoryMetric
          label="待处理"
          value={
            loading
              ? "…"
              : String(
                  statusCounts.missing +
                    statusCounts.conflict +
                    statusCounts.needsRepair,
                )
          }
          detail="缺失、分叉或元数据待修"
        />
      </section>

      {!!report?.warnings.length && (
        <section className="history-warning-strip" aria-label="扫描提示">
          <WarningCircle size={18} weight="fill" />
          <span>{report.warnings.length} 项历史元数据需要复查。</span>
        </section>
      )}

      <section className="history-layout">
        <aside className="history-home-panel">
          <div className="card-heading">
            <div>
              <p className="section-kicker">项目</p>
              <h2>项目</h2>
            </div>
            <FolderOpen size={22} />
          </div>
          <div className="history-project-list">
            {report?.projects.map((project) => (
              <ProjectRow
                active={project.id === selectedProjectId}
                disabled={disabled}
                key={project.id}
                onSelect={() => selectProject(project.id)}
                project={project}
              />
            ))}
            {!loading && !report?.projects.length && (
              <p className="muted-copy">还没有发现 Codex 项目历史。</p>
            )}
            {loading && <p className="muted-copy">正在扫描 Codex 项目…</p>}
          </div>

          <div className="history-home-compact" aria-label="来源 home">
            <div className="card-heading compact">
              <div>
                <p className="section-kicker">来源</p>
                <h2>来源</h2>
              </div>
              <Database size={18} />
            </div>
            {report?.homes.map((home) => (
              <div className="history-home-row compact" key={home.id}>
                <div>
                  <strong>{home.label}</strong>
                  <span>{home.sync_target ? "同步目标" : "仅作来源"}</span>
                </div>
                <small>{home.session_count} 个会话</small>
              </div>
            ))}
          </div>
        </aside>

        <section className="history-session-panel">
          <div className="card-heading">
            <div>
              <p className="section-kicker">会话历史</p>
              <h2>{selectedProject?.name ?? "官方历史"}</h2>
              {selectedProject?.cwd && <code>{selectedProject.cwd}</code>}
            </div>
            <div className="history-bulk-actions">
              <button
                className="quiet-button"
                disabled={disabled || !report?.sessions.length}
                type="button"
                onClick={toggleVisibleSessions}
              >
                {visibleAllSelected ? "取消全选" : "全选"}
              </button>
              <button
                className="quiet-button"
                disabled={disabled || selectedCount === 0}
                type="button"
                onClick={() => void exportHistory("sessions")}
              >
                导出选中
              </button>
              <button
                className="quiet-button"
                disabled={disabled || !selectedProjectId}
                type="button"
                onClick={() => void exportHistory("project")}
              >
                导出项目
              </button>
              <button
                className="danger-button"
                disabled={disabled || selectedCount === 0}
                type="button"
                onClick={() =>
                  setDeleteIntent({
                    scope: "sessions",
                    sessionIds: [...selectedSessionIds],
                    projectId: null,
                    label: "选中的会话",
                    count: selectedCount,
                  })
                }
              >
                <Trash size={16} /> 删除选中
              </button>
              <button
                className="danger-button"
                disabled={disabled || !selectedProject}
                type="button"
                onClick={() =>
                  selectedProject &&
                  setDeleteIntent({
                    scope: "project",
                    sessionIds: [],
                    projectId: selectedProject.id,
                    label: selectedProject.name,
                    count: selectedProject.session_count,
                  })
                }
              >
                删除项目
              </button>
            </div>
          </div>
          <div className="history-session-list">
            {report?.sessions.map((session) => (
              <HistorySessionRow
                checked={selectedSessionIds.has(session.id)}
                disabled={disabled}
                key={session.id}
                onToggle={() => toggleSession(session.id)}
                session={session}
              />
            ))}
            {!loading && !report?.sessions.length && (
              <EmptyState
                compact
                description="完成一次 Codex 任务或导入历史后，会话会按项目显示在这里。"
                icon={<Database size={20} />}
                title="这个项目还没有会话历史"
              />
            )}
            {loading && <p className="muted-copy">正在读取会话历史…</p>}
          </div>
        </section>
      </section>

      <Dialog
        busy={working}
        description={`将 ${deleteIntent?.count ?? 0} 个会话从 Codex 可见历史中移除，并先备份到 Relay 回收站。`}
        footer={
          <>
            <Button
              disabled={working}
              variant="secondary"
              onClick={() => setDeleteIntent(null)}
            >
              取消
            </Button>
            <Button
              loading={working}
              loadingLabel="正在移入回收站"
              leadingIcon={<Trash size={16} />}
              variant="danger"
              onClick={() => void confirmDelete()}
            >
              移入回收站
            </Button>
          </>
        }
        open={!!deleteIntent}
        size="sm"
        title={`删除 ${deleteIntent?.label ?? "会话历史"}`}
        onClose={() => setDeleteIntent(null)}
      >
        <p>
          这个操作不会删除账号凭据、配置、Keychain 或模型目录。回收站暂不提供 UI
          恢复，但会保留备份文件作为安全兜底。
        </p>
      </Dialog>
    </div>
  );
}

function HistoryMetric({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <article className="history-metric">
      <span>{label}</span>
      <strong>{value}</strong>
      <small>{detail}</small>
    </article>
  );
}

function ProjectRow({
  active,
  disabled,
  onSelect,
  project,
}: {
  active: boolean;
  disabled: boolean;
  onSelect: () => void;
  project: CodexHistoryProjectSummary;
}) {
  const pending =
    project.missing_count + project.conflict_count + project.needs_repair_count;
  return (
    <button
      className={`history-project-row ${active ? "active" : ""}`.trim()}
      disabled={disabled}
      type="button"
      onClick={onSelect}
    >
      <div>
        <strong>{project.name}</strong>
        <span>
          {project.session_count} 个会话 · {formatDate(project.updated_at_ms)}
        </span>
      </div>
      {project.cwd && <code title={project.cwd}>{project.cwd}</code>}
      <small>{pending > 0 ? `${pending} 项待处理` : "一致"}</small>
    </button>
  );
}

function HistorySessionRow({
  checked,
  disabled,
  onToggle,
  session,
}: {
  checked: boolean;
  disabled: boolean;
  onToggle: () => void;
  session: CodexHistorySessionSummary;
}) {
  return (
    <article className="history-session-row selectable">
      <label className="history-session-check">
        <input
          checked={checked}
          disabled={disabled}
          type="checkbox"
          onChange={onToggle}
        />
      </label>
      <div className="history-session-main">
        <div className="history-session-title">
          <h2>{session.title ?? "未命名 Codex 会话"}</h2>
          <StatusPill compact tone={historyStatusTone(session.status)}>
            {historyStatusLabel(session.status)}
          </StatusPill>
        </div>
        <p>
          {shortId(session.id)} · {formatDate(session.updated_at_ms)} ·{" "}
          {session.source_count} 个来源
        </p>
        {session.cwd && <code title={session.cwd}>{session.cwd}</code>}
      </div>
      <div className="history-session-meta">
        {session.missing_target_count > 0 && (
          <span>{session.missing_target_count} 个目标缺失</span>
        )}
        {session.divergent_source_count > 0 && (
          <span>{session.divergent_source_count + 1} 个分叉版本</span>
        )}
      </div>
      <details className="history-source-details">
        <summary>来源明细</summary>
        <div className="history-source-list">
          {session.sources.map((source) => (
            <div
              className="history-source-row"
              key={`${source.home_id}:${source.sha256}`}
            >
              <strong>{source.home_label}</strong>
              <span>
                {formatDate(source.updated_at_ms)} ·{" "}
                {source.event_count > 0 ? `${source.event_count} events` : "摘要"} ·{" "}
                {source.archived ? "已归档" : "活跃"}
              </span>
              <code title={source.rollout_path}>{source.rollout_path}</code>
            </div>
          ))}
        </div>
      </details>
    </article>
  );
}

function historyStatusTone(status: string) {
  if (status === "consistent") return "success" as const;
  if (status === "missing") return "warning" as const;
  if (status === "conflict") return "danger" as const;
  return "neutral" as const;
}

function historyStatusLabel(status: string) {
  return (
    {
      consistent: "一致",
      missing: "缺失",
      conflict: "分叉",
      needs_repair: "待修复",
    }[status] ?? "待检查"
  );
}

function shortId(value: string) {
  return value.slice(0, 8);
}

function formatDate(value: number) {
  if (!value) return "未知时间";
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value));
}

function sanitizeFileName(value: string) {
  return value
    .replace(/[\\/:*?"<>|]+/g, "-")
    .replace(/\s+/g, "-")
    .slice(0, 80);
}
