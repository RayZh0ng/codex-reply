import {
  ArrowRight,
  ChartLineUp,
  Lightning,
  Plus,
  ShieldCheck,
} from "@phosphor-icons/react";
import { open } from "@tauri-apps/plugin-dialog";
import { Bar, BarChart, ResponsiveContainer, Tooltip, XAxis } from "recharts";
import { useState } from "react";

import type {
  DashboardSnapshot,
  DesktopWorkspaceMode,
  ManagedTaskStatus,
  StartManagedTaskInput,
} from "../../shared/ipc";

interface DashboardProps {
  snapshot: DashboardSnapshot;
  taskStatus: ManagedTaskStatus;
  busy: boolean;
  onNavigate: (page: "profiles" | "gateway" | "notifications") => void;
  onStartTask: (input: StartManagedTaskInput) => Promise<void>;
  onRequestCancelTask: () => void;
  workspaceMode?: DesktopWorkspaceMode;
}

function Metric({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <article className="metric-card">
      <p>{label}</p>
      <strong>{value}</strong>
      <span>{detail}</span>
    </article>
  );
}

function ManagedTaskCard({
  snapshot,
  taskStatus,
  busy,
  onNavigate,
  onStartTask,
  onRequestCancelTask,
  workspaceMode = "per_profile",
}: DashboardProps) {
  const [instruction, setInstruction] = useState("");
  const [workingDirectory, setWorkingDirectory] = useState<string | null>(null);
  const currentProfile = snapshot.profiles.find((profile) => profile.is_current);
  const supportsTask =
    currentProfile?.kind === "codex_oauth" &&
    currentProfile.enabled &&
    currentProfile.credential_configured;
  const isRunning = taskStatus.phase === "running";
  const activeProfile = snapshot.profiles.find(
    (profile) => profile.id === taskStatus.profile_id,
  );

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
    <article
      className="surface-card managed-task-card"
      aria-labelledby="managed-task-title"
    >
      <div className="card-heading">
        <div>
          <p className="section-kicker">Managed Codex</p>
          <h2 id="managed-task-title">受管 Codex 任务</h2>
        </div>
        <span className={`status-pill ${isRunning ? "success" : "neutral"}`}>
          <i /> {isRunning ? "运行中" : taskPhaseLabel(taskStatus.phase)}
        </span>
      </div>
      <p className="managed-task-copy">
        {isRunning
          ? `正在使用“${activeProfile?.alias ?? "已选择档案"}”运行受管任务。`
          : taskStatus.message}
      </p>
      {isRunning ? (
        <button
          className="danger-button"
          type="button"
          disabled={busy}
          onClick={onRequestCancelTask}
        >
          停止任务
        </button>
      ) : supportsTask ? (
        <div className="managed-task-form">
          <label>
            任务说明
            <textarea
              value={instruction}
              onChange={(event) => setInstruction(event.target.value)}
              placeholder="描述希望 Codex 在所选工作目录中完成的任务"
              rows={3}
            />
          </label>
          <div className="managed-task-directory">
            <span>{workingDirectory ? "已选择工作目录" : "尚未选择工作目录"}</span>
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
            className="primary-button"
            type="button"
            disabled={busy || !workingDirectory || !instruction.trim()}
            onClick={startTask}
          >
            启动受管任务
          </button>
        </div>
      ) : (
        <div className="managed-task-unavailable">
          <p>请先选择一个已授权且启用的 OAuth 档案。</p>
          <button
            className="text-button"
            type="button"
            onClick={() => onNavigate("profiles")}
          >
            前往档案
          </button>
        </div>
      )}
      <p className="managed-task-note">
        任务仅控制由 Relay 启动的 CLI 子进程。切换当前档案会投影已保存的凭据到默认
        .codex/auth.json，并启动{workspaceModeLabel(workspaceMode)}；不会再次打开
        OAuth， 也不会迁移 ChatGPT Chat/Work 会话。
      </p>
    </article>
  );
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

function workspaceModeLabel(mode: DesktopWorkspaceMode) {
  return {
    fresh: "本次全新 ChatGPT/Codex 工作区",
    per_profile: "档案专属的 ChatGPT/Codex 工作区",
    shared: "共享原客户端状态的 ChatGPT/Codex",
  }[mode];
}

export function Dashboard({
  snapshot,
  taskStatus,
  busy,
  onNavigate,
  onStartTask,
  onRequestCancelTask,
  workspaceMode,
}: DashboardProps) {
  const { gateway, metrics, profiles, notifications } = snapshot;
  const trend = [
    { day: "一", requests: 0 },
    { day: "二", requests: 0 },
    { day: "三", requests: 0 },
    { day: "四", requests: 0 },
    { day: "五", requests: metrics.total_requests },
    { day: "六", requests: 0 },
    { day: "日", requests: 0 },
  ];
  const availableChannels = notifications.filter((channel) => channel.enabled).length;

  return (
    <div className="page dashboard-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <p className="section-kicker">Overview</p>
          <h1>今天，服务一切就绪。</h1>
          <p className="page-subtitle">
            在这里查看本机网关、账号池与通知投递的安全状态。
          </p>
        </div>
        <button
          className="quiet-button"
          type="button"
          onClick={() => onNavigate("profiles")}
        >
          <Plus size={18} weight="bold" /> 添加档案
        </button>
      </header>

      <section
        className="gateway-hero"
        data-animate="hero"
        aria-labelledby="gateway-heading"
      >
        <div
          className={`hero-orb ${gateway.running ? "is-live" : ""}`}
          aria-hidden="true"
        >
          <Lightning size={28} weight="fill" />
        </div>
        <div className="hero-content">
          <span className={`status-pill ${gateway.running ? "success" : "neutral"}`}>
            <i /> {gateway.running ? "网关运行中" : "网关未启动"}
          </span>
          <h2 id="gateway-heading">
            {gateway.running ? "本机 API 已受到保护" : "先配置一个安全的本机网关"}
          </h2>
          <p>
            {gateway.running
              ? `监听 ${gateway.bind_mode === "lan" ? "受控局域网" : "仅本机回环"} · ${gateway.available_profiles} 个可用成员`
              : "默认只允许本机回环访问；启用局域网时会要求白名单与客户端 Key。"}
          </p>
          <button
            className="primary-button"
            type="button"
            onClick={() => onNavigate("gateway")}
          >
            管理网关 <ArrowRight size={17} weight="bold" />
          </button>
        </div>
        <div className="hero-health" aria-label="网关健康摘要">
          <div>
            <span>可用成员</span>
            <strong>{gateway.available_profiles}</strong>
          </div>
          <div>
            <span>冷却中</span>
            <strong>{gateway.cooling_profiles}</strong>
          </div>
          <div>
            <span>客户端 Key</span>
            <strong>{gateway.client_key_count}</strong>
          </div>
        </div>
      </section>

      <section className="metrics-grid" data-animate="metrics">
        <Metric
          label="总请求数"
          value={String(metrics.total_requests)}
          detail={`${metrics.successful_requests} 次成功 · ${metrics.failed_requests} 次失败`}
        />
        <Metric
          label="估算 Token"
          value={metrics.estimated_tokens.toLocaleString()}
          detail="仅统计本应用网关流量"
        />
        <Metric
          label="平均延迟"
          value={metrics.average_latency_ms ? `${metrics.average_latency_ms} ms` : "—"}
          detail="不会记录请求或响应正文"
        />
        <Metric
          label="通知频道"
          value={String(availableChannels)}
          detail="仅发送脱敏状态摘要"
        />
      </section>

      <section className="dashboard-lower" data-animate="lower">
        <article className="surface-card usage-card">
          <div className="card-heading">
            <div>
              <p className="section-kicker">Activity</p>
              <h2>请求趋势</h2>
            </div>
            <ChartLineUp size={22} />
          </div>
          <div className="chart-wrap">
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={trend} barSize={18}>
                <XAxis dataKey="day" axisLine={false} tickLine={false} />
                <Tooltip cursor={{ fill: "rgba(31, 100, 214, .05)" }} />
                <Bar dataKey="requests" fill="#207ce8" radius={[7, 7, 3, 3]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        </article>
        <article className="surface-card pool-card">
          <div className="card-heading">
            <div>
              <p className="section-kicker">Account pool</p>
              <h2>账号池</h2>
            </div>
            <button
              className="text-button"
              type="button"
              onClick={() => onNavigate("profiles")}
            >
              查看全部
            </button>
          </div>
          {profiles.length ? (
            <div className="profile-summary-list">
              {profiles.slice(0, 3).map((profile) => (
                <div className="profile-summary" key={profile.id}>
                  <div className="profile-avatar">
                    {profile.alias.slice(0, 1).toUpperCase()}
                  </div>
                  <div>
                    <strong>{profile.alias}</strong>
                    <span>{profile.models.join(" · ") || "未声明模型"}</span>
                  </div>
                  <span
                    className={`health-dot ${profile.health}`}
                    title={profile.health}
                  />
                </div>
              ))}
            </div>
          ) : (
            <div className="empty-inline">
              <ShieldCheck size={22} weight="duotone" />
              <p>还没有可用档案。添加一个 API Key 档案后即可加入账号池。</p>
              <button
                className="text-button"
                type="button"
                onClick={() => onNavigate("profiles")}
              >
                添加档案
              </button>
            </div>
          )}
        </article>
      </section>
      <section className="managed-task-section" data-animate="lower">
        <ManagedTaskCard
          snapshot={snapshot}
          taskStatus={taskStatus}
          busy={busy}
          onNavigate={onNavigate}
          onStartTask={onStartTask}
          onRequestCancelTask={onRequestCancelTask}
          workspaceMode={workspaceMode}
        />
      </section>
    </div>
  );
}
