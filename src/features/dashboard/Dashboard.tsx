import { ArrowRight } from "@phosphor-icons/react/ArrowRight";
import { ChatCircleDots } from "@phosphor-icons/react/ChatCircleDots";
import { FolderOpen } from "@phosphor-icons/react/FolderOpen";
import { Lightning } from "@phosphor-icons/react/Lightning";
import { Play } from "@phosphor-icons/react/Play";
import { Power } from "@phosphor-icons/react/Power";
import { ShieldCheck } from "@phosphor-icons/react/ShieldCheck";
import { TerminalWindow } from "@phosphor-icons/react/TerminalWindow";
import { UsersThree } from "@phosphor-icons/react/UsersThree";
import { open } from "@tauri-apps/plugin-dialog";
import { useState } from "react";

import type {
  DashboardSnapshot,
  ManagedTaskStatus,
  StartManagedTaskInput,
} from "../../shared/ipc";
import { PageHeader, StatusPill, type StatusTone } from "../../shared/ui";
import "./dashboard.css";

interface DashboardProps {
  snapshot: DashboardSnapshot;
  taskStatus: ManagedTaskStatus;
  busy: boolean;
  onNavigate: (page: "profiles" | "gateway" | "collaboration") => void;
  onStartTask: (input: StartManagedTaskInput) => Promise<void>;
  onRequestCancelTask: () => void;
}

function taskPhaseLabel(phase: ManagedTaskStatus["phase"]) {
  return {
    idle: "空闲",
    completed: "已完成",
    failed: "未完成",
    cancelled: "已停止",
    running: "运行中",
  }[phase];
}

function taskPhaseTone(phase: ManagedTaskStatus["phase"]): StatusTone {
  const tones: Record<ManagedTaskStatus["phase"], StatusTone> = {
    idle: "neutral",
    completed: "success",
    failed: "danger",
    cancelled: "warning",
    running: "success",
  };
  return tones[phase];
}

function ManagedTaskWorkbench({
  snapshot,
  taskStatus,
  busy,
  onNavigate,
  onStartTask,
  onRequestCancelTask,
}: DashboardProps) {
  const [instruction, setInstruction] = useState("");
  const [workingDirectory, setWorkingDirectory] = useState<string | null>(null);
  const currentProfile = snapshot.profiles.find((profile) => profile.is_current);
  const activeProfile = snapshot.profiles.find(
    (profile) => profile.id === taskStatus.profile_id,
  );
  const supportsTask =
    currentProfile?.kind === "codex_oauth" &&
    currentProfile.enabled &&
    currentProfile.credential_configured;
  const isRunning = taskStatus.phase === "running";
  const hasOutcome = taskStatus.phase !== "idle" && !isRunning;

  const selectWorkingDirectory = async () => {
    const selected = await open({
      title: "选择受管 Codex 任务的工作目录",
      directory: true,
      multiple: false,
    });
    if (typeof selected === "string") setWorkingDirectory(selected);
  };

  const startTask = () => {
    if (!workingDirectory || !instruction.trim()) return;
    const input = {
      instruction: instruction.trim(),
      working_directory: workingDirectory,
    };
    setInstruction("");
    setWorkingDirectory(null);
    void onStartTask(input);
  };

  return (
    <section className="task-workbench" aria-labelledby="task-workbench-title">
      <header className="task-workbench-header">
        <div className="task-workbench-title">
          <span className="task-workbench-icon" aria-hidden="true">
            <TerminalWindow size={21} weight="duotone" />
          </span>
          <div>
            <h2 id="task-workbench-title">受管 Codex 任务</h2>
            <p>在本机项目中启动一项可随时停止的 Codex 工作。</p>
          </div>
        </div>
        <StatusPill
          aria-label={`任务状态：${taskPhaseLabel(taskStatus.phase)}`}
          tone={taskPhaseTone(taskStatus.phase)}
        >
          {taskPhaseLabel(taskStatus.phase)}
        </StatusPill>
      </header>

      {isRunning ? (
        <div className="task-execution-surface" role="status">
          <div className="task-execution-topline">
            <span>
              <i />
              Codex 正在执行
            </span>
            <code>{activeProfile?.alias ?? "已选择档案"}</code>
          </div>
          <div className="task-execution-body">
            <TerminalWindow size={30} weight="duotone" aria-hidden="true" />
            <div>
              <span>当前状态</span>
              <strong>{taskStatus.message}</strong>
            </div>
          </div>
          <div className="task-execution-footer">
            <p>停止操作会先要求确认，并仅终止由 Relay 启动的任务进程。</p>
            <button
              className="task-stop-button"
              type="button"
              disabled={busy}
              onClick={onRequestCancelTask}
            >
              <Power size={17} weight="bold" />
              停止任务
            </button>
          </div>
        </div>
      ) : (
        <>
          {hasOutcome && (
            <div
              className={`task-outcome is-${taskPhaseTone(taskStatus.phase)}`}
              role="status"
            >
              <strong>{taskPhaseLabel(taskStatus.phase)}</strong>
              <p>{taskStatus.message}</p>
            </div>
          )}
          {supportsTask ? (
            <div className="managed-task-form">
              <label className="task-instruction-field">
                <span>任务说明</span>
                <textarea
                  value={instruction}
                  onChange={(event) => setInstruction(event.target.value)}
                  placeholder="描述希望 Codex 在所选工作目录中完成的任务…"
                  rows={6}
                />
              </label>
              <div className="task-composer-footer">
                <div className="managed-task-directory">
                  <FolderOpen size={18} aria-hidden="true" />
                  <div>
                    <span>工作目录</span>
                    <code title={workingDirectory ?? undefined}>
                      {workingDirectory ?? "尚未选择目录"}
                    </code>
                  </div>
                  <button
                    className="quiet-button"
                    type="button"
                    disabled={busy}
                    onClick={() => void selectWorkingDirectory()}
                  >
                    选择目录
                  </button>
                </div>
                <button
                  className="primary-button task-start-button"
                  type="button"
                  disabled={busy || !workingDirectory || !instruction.trim()}
                  onClick={startTask}
                >
                  <Play size={17} weight="fill" />
                  启动受管任务
                </button>
              </div>
            </div>
          ) : (
            <div className="managed-task-unavailable">
              <div>
                <strong>当前档案不能启动受管任务</strong>
                <p>请选择一个已授权、已启用的 OAuth 档案。</p>
              </div>
              <button
                className="quiet-button"
                type="button"
                onClick={() => onNavigate("profiles")}
              >
                前往档案
                <ArrowRight size={16} weight="bold" />
              </button>
            </div>
          )}
        </>
      )}

      <footer className="task-workbench-note">
        <ShieldCheck size={15} weight="fill" aria-hidden="true" />
        <span>
          Relay 仅控制其启动的 CLI 子进程；档案切换继续复用原 Codex
          客户端数据目录与本机历史。
        </span>
      </footer>
    </section>
  );
}

function formatLatency(value: number | null | undefined) {
  return value == null ? "—" : `${value.toLocaleString()} ms`;
}

function formatBytes(value: number | undefined) {
  if (!value) return "0 B";
  if (value >= 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KiB`;
  return `${value} B`;
}

export function Dashboard({
  snapshot,
  taskStatus,
  busy,
  onNavigate,
  onStartTask,
  onRequestCancelTask,
}: DashboardProps) {
  const { collaboration, gateway, metrics, profiles } = snapshot;
  const currentProfile = profiles.find((profile) => profile.is_current);
  const poolProfiles = profiles.filter((profile) => profile.in_pool);
  const healthyProfiles = poolProfiles.filter(
    (profile) => profile.health === "healthy",
  );

  return (
    <div className="page dashboard-page">
      <PageHeader
        actions={
          <div className="dashboard-current-profile" aria-label="当前档案">
            <span>当前档案</span>
            <strong title={currentProfile?.alias ?? "未选择"}>
              {currentProfile?.alias ?? "未选择"}
            </strong>
          </div>
        }
        className="dashboard-heading"
        description="启动本机 Codex 任务，并在同一处确认执行状态与基础设施可用性。"
        title="任务工作台"
      />

      <ManagedTaskWorkbench
        snapshot={snapshot}
        taskStatus={taskStatus}
        busy={busy}
        onNavigate={onNavigate}
        onStartTask={onStartTask}
        onRequestCancelTask={onRequestCancelTask}
      />

      <section className="dashboard-support-grid" aria-label="工作台状态摘要">
        <article className="dashboard-summary-panel infrastructure-panel">
          <div className="dashboard-panel-heading">
            <div>
              <h2>基础设施</h2>
              <p>任务运行所依赖的本机服务。</p>
            </div>
            <Lightning size={20} weight="duotone" aria-hidden="true" />
          </div>
          <dl className="dashboard-definition-list">
            <div>
              <dt>API 网关</dt>
              <dd>
                <span className={gateway.running ? "is-positive" : ""}>
                  {gateway.running ? "运行中" : "未启动"}
                </span>
                <small title={gateway.service_url}>{gateway.service_url}</small>
              </dd>
            </div>
            <div>
              <dt>账号池</dt>
              <dd>
                <span>
                  {healthyProfiles.length} / {poolProfiles.length} 健康
                </span>
                <small>{gateway.cooling_profiles} 个冷却中</small>
              </dd>
            </div>
            <div>
              <dt>客户端访问</dt>
              <dd>
                <span>{gateway.client_key_count} 个 Key</span>
                <small>{gateway.certificate_ready ? "证书就绪" : "证书待生成"}</small>
              </dd>
            </div>
          </dl>
          <button
            className="text-button dashboard-panel-link"
            type="button"
            onClick={() => onNavigate("gateway")}
          >
            管理网关 <ArrowRight size={15} weight="bold" />
          </button>
        </article>

        <article className="dashboard-summary-panel activity-panel">
          <div className="dashboard-panel-heading">
            <div>
              <h2>Relay 流量</h2>
              <p>仅统计经过本应用网关的请求。</p>
            </div>
          </div>
          <dl className="dashboard-metrics-list dashboard-performance-grid">
            <div>
              <dt>近 {metrics.window_minutes ?? 60} 分钟请求</dt>
              <dd>
                {(metrics.window_requests ?? metrics.total_requests).toLocaleString()}
              </dd>
              <small>
                成功率{" "}
                {metrics.window_success_rate == null
                  ? "—"
                  : `${(metrics.window_success_rate * 100).toFixed(1)}%`}{" "}
                · {(metrics.requests_per_minute ?? 0).toFixed(2)} req/min
              </small>
            </div>
            <div>
              <dt>总延迟 P50 / P95 / P99</dt>
              <dd>{formatLatency(metrics.latency_p50_ms)}</dd>
              <small>
                {formatLatency(metrics.latency_p95_ms)} ·{" "}
                {formatLatency(metrics.latency_p99_ms)}
              </small>
            </div>
            <div>
              <dt>TTFB P50 / P95 / P99</dt>
              <dd>{formatLatency(metrics.ttfb_p50_ms)}</dd>
              <small>
                {formatLatency(metrics.ttfb_p95_ms)} ·{" "}
                {formatLatency(metrics.ttfb_p99_ms)}
              </small>
            </div>
            <div>
              <dt>并发 / 排队</dt>
              <dd>
                {metrics.active_requests ?? 0} / {metrics.queued_requests ?? 0}
              </dd>
              <small>{metrics.retry_count ?? 0} 次内部重试</small>
            </div>
            <div>
              <dt>传输量</dt>
              <dd>{formatBytes(metrics.response_bytes)}</dd>
              <small>入站 {formatBytes(metrics.request_bytes)} · 不记录正文</small>
            </div>
            <div>
              <dt>累计 Token</dt>
              <dd>{metrics.estimated_tokens.toLocaleString()}</dd>
              <small>遥测丢弃 {metrics.telemetry_dropped ?? 0} 条</small>
            </div>
          </dl>
        </article>

        <article className="dashboard-summary-panel access-panel">
          <div className="dashboard-panel-heading">
            <div>
              <h2>资源入口</h2>
              <p>管理任务使用的档案与协作连接。</p>
            </div>
          </div>
          <button
            className="dashboard-resource-row"
            type="button"
            onClick={() => onNavigate("profiles")}
          >
            <span className="dashboard-resource-icon">
              <UsersThree size={18} />
            </span>
            <span>
              <strong>档案与账号池</strong>
              <small>
                {profiles.length} 个档案 · {poolProfiles.length} 个池成员
              </small>
            </span>
            <ArrowRight size={15} />
          </button>
          <button
            className="dashboard-resource-row"
            type="button"
            onClick={() => onNavigate("collaboration")}
          >
            <span className="dashboard-resource-icon">
              <ChatCircleDots size={18} />
            </span>
            <span>
              <strong>群聊协作</strong>
              <small>
                {collaboration.enabled_bots} 个机器人 · {collaboration.active_sessions}{" "}
                个活动会话
              </small>
            </span>
            <ArrowRight size={15} />
          </button>
        </article>
      </section>
    </div>
  );
}
