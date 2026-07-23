import { useGSAP } from "@gsap/react";
import {
  Bell,
  ChartPieSlice,
  GearSix,
  Lightning,
  List,
  ShieldWarning,
  UsersThree,
} from "@phosphor-icons/react";
import gsap from "gsap";
import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";

import { Dashboard } from "../features/dashboard/Dashboard";
import { Gateway } from "../features/gateway/Gateway";
import { Notifications } from "../features/notifications/Notifications";
import { Profiles } from "../features/profiles/Profiles";
import { Settings } from "../features/settings/Settings";
import {
  api,
  type CurrentProfileActivation,
  type DesktopWorkspaceHistoryItem,
  type DesktopWorkspaceMode,
  type DesktopWorkspaceSettings,
  type DashboardSnapshot,
  type ManagedTaskStatus,
  type StartManagedTaskInput,
  RelayError,
} from "../shared/ipc";
import logo from "../../assets/codex-relay-icon-modern.png";
import "./App.css";

type Page = "dashboard" | "profiles" | "gateway" | "notifications" | "settings";
type Confirmation = {
  title: string;
  detail: string;
  confirmLabel: string;
  successMessage?: string;
  action: () => Promise<void>;
} | null;

const idleTaskStatus: ManagedTaskStatus = {
  phase: "idle",
  profile_id: null,
  message: "当前没有正在运行的受管 Codex 任务。",
};
const PROFILE_ACTIVATION_TIMEOUT_MS = 20_000;
const defaultWorkspaceSettings: DesktopWorkspaceSettings = { mode: "per_profile" };

gsap.registerPlugin(useGSAP);

function App() {
  const root = useRef<HTMLElement>(null);
  const quotaRefreshInFlight = useRef(false);
  const initialQuotaRefreshStarted = useRef(false);
  const hasAuthorizedOAuthProfiles = useRef(false);
  const [page, setPage] = useState<Page>("dashboard");
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [taskStatus, setTaskStatus] = useState<ManagedTaskStatus>(idleTaskStatus);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionBusy, setActionBusy] = useState(false);
  const [confirmation, setConfirmation] = useState<Confirmation>(null);
  const [profileActivation, setProfileActivation] =
    useState<CurrentProfileActivation | null>(null);
  const [workspaceSettings, setWorkspaceSettings] = useState<DesktopWorkspaceSettings>(
    defaultWorkspaceSettings,
  );
  const [workspaceHistory, setWorkspaceHistory] = useState<
    DesktopWorkspaceHistoryItem[]
  >([]);

  const busy = actionBusy || profileActivation?.status === "switching";
  const refresh = useCallback(async () => {
    try {
      const [
        nextSnapshot,
        nextTaskStatus,
        nextWorkspaceSettings,
        nextWorkspaceHistory,
      ] = await Promise.all([
        api.dashboard(),
        api.managedTaskStatus(),
        api.desktopWorkspaceSettings().catch(() => defaultWorkspaceSettings),
        api.listDesktopWorkspaces().catch(() => []),
      ]);
      setSnapshot(nextSnapshot);
      setTaskStatus(nextTaskStatus);
      setWorkspaceSettings(nextWorkspaceSettings);
      setWorkspaceHistory(nextWorkspaceHistory);
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);
  hasAuthorizedOAuthProfiles.current =
    snapshot?.profiles.some(
      (profile) => profile.kind === "codex_oauth" && profile.credential_configured,
    ) ?? false;
  const refreshQuotaSummaries = useCallback(async () => {
    if (quotaRefreshInFlight.current || !hasAuthorizedOAuthProfiles.current) return;
    quotaRefreshInFlight.current = true;
    try {
      await api.refreshProfileQuotas();
      await refresh();
    } catch {
      // Per-profile cached states carry refresh failures; do not replace the whole app with an error.
    } finally {
      quotaRefreshInFlight.current = false;
    }
  }, [refresh]);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  useEffect(() => {
    if (!snapshot || initialQuotaRefreshStarted.current) return;
    initialQuotaRefreshStarted.current = true;
    void refreshQuotaSummaries();
  }, [snapshot, refreshQuotaSummaries]);
  useEffect(() => {
    const refreshWhenVisible = () => {
      if (document.visibilityState === "visible") void refreshQuotaSummaries();
    };
    window.addEventListener("focus", refreshQuotaSummaries);
    document.addEventListener("visibilitychange", refreshWhenVisible);
    const timer = window.setInterval(() => void refreshQuotaSummaries(), 60_000);
    return () => {
      window.removeEventListener("focus", refreshQuotaSummaries);
      document.removeEventListener("visibilitychange", refreshWhenVisible);
      window.clearInterval(timer);
    };
  }, [refreshQuotaSummaries]);
  useEffect(() => {
    if (taskStatus.phase !== "running") return;
    const timer = window.setInterval(() => {
      void api
        .managedTaskStatus()
        .then(setTaskStatus)
        .catch((reason) => setError(errorMessage(reason)));
    }, 2000);
    return () => window.clearInterval(timer);
  }, [taskStatus.phase]);
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(timer);
  }, [notice]);
  useGSAP(
    () => {
      const motion = gsap.matchMedia();
      motion.add({ reduce: "(prefers-reduced-motion: reduce)" }, (context) => {
        if (context.conditions?.reduce) return undefined;
        const timeline = gsap.timeline({ defaults: { ease: "power3.out" } });
        timeline
          .from("[data-animate='heading']", { autoAlpha: 0, y: 14, duration: 0.34 })
          .from(
            "[data-animate='hero'], [data-animate='toolbar'], [data-animate='gateway'], [data-animate='notice']",
            { autoAlpha: 0, y: 14, duration: 0.32 },
            "-=0.14",
          )
          .from(
            "[data-animate='metrics'] > *, [data-animate='cards'] > *, [data-animate='lower'] > *",
            { autoAlpha: 0, y: 12, duration: 0.28, stagger: 0.055 },
            "-=0.1",
          );
        return () => timeline.kill();
      });
      return () => motion.revert();
    },
    { scope: root, dependencies: [page, snapshot], revertOnUpdate: true },
  );

  const execute = async (action: () => Promise<unknown>, message?: string) => {
    setActionBusy(true);
    try {
      await action();
      await refresh();
      if (message) setNotice(message);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setActionBusy(false);
    }
  };
  const selectProfileDirect = async (id: string, confirmedDesktopRestart = false) => {
    setActionBusy(true);
    try {
      const activation = await api.selectProfile(id, confirmedDesktopRestart);
      if (activation.status === "switching") {
        setProfileActivation(activation);
        return;
      }
      await refresh();
      setNotice(activation.message);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setActionBusy(false);
    }
  };
  const selectProfile = async (id: string) => {
    if (workspaceSettings.mode === "shared") {
      setConfirmation({
        title: "关闭并重启原客户端？",
        detail:
          "共享模式会先请求正常退出 ChatGPT/Codex，再使用你原有的客户端数据目录启动。已保存的会话、设置与状态会保留，但未发送内容可能丢失。",
        confirmLabel: "关闭并切换",
        action: () => selectProfileDirect(id, true),
      });
      return;
    }
    await selectProfileDirect(id);
  };
  const startManagedTask = async (input: StartManagedTaskInput) => {
    setActionBusy(true);
    try {
      const status = await api.startManagedTask(input);
      await refresh();
      setNotice(status.message);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setActionBusy(false);
    }
  };
  useEffect(() => {
    const attemptId = profileActivation?.attempt_id;
    if (profileActivation?.status !== "switching" || !attemptId) return;
    let timedOut = false;
    const poll = () => {
      if (timedOut) return;
      void api
        .currentProfileActivationStatus(attemptId)
        .then(async (next) => {
          if (timedOut) return;
          setProfileActivation(next);
          if (next.status !== "switching") {
            await refresh();
            setNotice(next.message);
          }
        })
        .catch((reason) => {
          if (!timedOut) setError(errorMessage(reason));
        });
    };
    poll();
    const timer = window.setInterval(poll, 1000);
    const deadline = window.setTimeout(() => {
      timedOut = true;
      setProfileActivation((current) =>
        current?.attempt_id === attemptId
          ? {
              ...current,
              status: "failed",
              message: "账号切换状态超时。请确认 ChatGPT/Codex 已退出后重试。",
            }
          : current,
      );
      setError("账号切换状态超时。请确认 ChatGPT/Codex 已退出后重试。");
    }, PROFILE_ACTIVATION_TIMEOUT_MS);
    return () => {
      window.clearInterval(timer);
      window.clearTimeout(deadline);
    };
  }, [profileActivation?.attempt_id, profileActivation?.status, refresh]);
  const requestCancelManagedTask = () =>
    setConfirmation({
      title: "停止正在运行的受管任务？",
      detail: "任务会立即终止，且不会保留任务说明、工作目录或模型输出。",
      confirmLabel: "停止任务",
      action: async () => {
        const status = await api.cancelManagedTask();
        await refresh();
        setNotice(status.message);
      },
    });
  const requestDelete = (title: string, detail: string, action: () => Promise<void>) =>
    setConfirmation({
      title,
      detail,
      confirmLabel: "确认删除",
      successMessage: "操作已完成。",
      action,
    });
  const content = snapshot ? (
    <PageContent
      page={page}
      snapshot={snapshot}
      taskStatus={taskStatus}
      busy={busy}
      navigate={setPage}
      execute={execute}
      selectProfile={selectProfile}
      startManagedTask={startManagedTask}
      requestCancelManagedTask={requestCancelManagedTask}
      requestDelete={requestDelete}
      notify={setNotice}
      workspaceSettings={workspaceSettings}
      workspaceHistory={workspaceHistory}
      onChangeWorkspaceMode={async (mode) => {
        await execute(
          () => api.updateDesktopWorkspaceSettings(mode),
          "客户端工作区模式已保存。",
        );
      }}
      onRestoreWorkspace={async (id) => {
        const activation = await api.restoreDesktopWorkspace(id);
        if (activation.status === "switching") {
          setProfileActivation(activation);
          return;
        }
        await refresh();
        setNotice(activation.message);
      }}
    />
  ) : (
    <LoadingState error={error} retry={refresh} />
  );

  return (
    <main ref={root} className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img src={logo} alt="" />
          <span>Codex Relay</span>
        </div>
        <nav aria-label="主导航">
          <NavItem
            active={page === "dashboard"}
            icon={<ChartPieSlice size={20} />}
            label="总览"
            onClick={() => setPage("dashboard")}
          />
          <NavItem
            active={page === "profiles"}
            icon={<UsersThree size={20} />}
            label="档案"
            onClick={() => setPage("profiles")}
          />
          <NavItem
            active={page === "gateway"}
            icon={<Lightning size={20} />}
            label="网关"
            onClick={() => setPage("gateway")}
          />
          <NavItem
            active={page === "notifications"}
            icon={<Bell size={20} />}
            label="通知"
            onClick={() => setPage("notifications")}
          />
        </nav>
        <div className="sidebar-bottom">
          <span className="desktop-status">
            <i /> 本机优先
          </span>
          <button
            className={`nav-item ${page === "settings" ? "active" : ""}`}
            type="button"
            onClick={() => setPage("settings")}
          >
            <GearSix size={20} /> 设置
          </button>
        </div>
      </aside>
      <section className="app-main">
        <header className="topbar">
          <div className="crumb">
            <List size={19} />
            <span>
              {page === "dashboard"
                ? "总览"
                : page === "profiles"
                  ? "档案"
                  : page === "gateway"
                    ? "网关"
                    : page === "notifications"
                      ? "通知"
                      : "设置"}
            </span>
          </div>
          <div className="topbar-status">
            {snapshot && (
              <>
                <span
                  className={`status-pill compact ${snapshot.gateway.running ? "success" : "neutral"}`}
                >
                  <i /> {snapshot.gateway.running ? "服务运行中" : "服务未启动"}
                </span>
                <button
                  className="refresh-button"
                  type="button"
                  onClick={() => void refresh()}
                  aria-label="刷新状态"
                >
                  刷新
                </button>
              </>
            )}
          </div>
        </header>
        {error && (
          <div className="error-banner" role="alert">
            <ShieldWarning size={20} weight="fill" />
            <div>
              <strong>无法读取本机状态</strong>
              <p>{error}</p>
            </div>
            <button
              className="text-button"
              type="button"
              onClick={() => {
                setError(null);
                void refresh();
              }}
            >
              重试
            </button>
          </div>
        )}
        {notice && (
          <div className="toast" role="status">
            {notice}
          </div>
        )}
        <div className="content-scroll">{content}</div>
      </section>
      {confirmation && (
        <ConfirmDialog
          confirmation={confirmation}
          busy={busy}
          close={() => setConfirmation(null)}
          execute={execute}
        />
      )}
      {profileActivation?.status === "switching" && (
        <ProfileActivationDialog activation={profileActivation} />
      )}
    </main>
  );
}

function PageContent({
  page,
  snapshot,
  taskStatus,
  busy,
  navigate,
  execute,
  selectProfile,
  startManagedTask,
  requestCancelManagedTask,
  requestDelete,
  notify,
  workspaceSettings,
  workspaceHistory,
  onChangeWorkspaceMode,
  onRestoreWorkspace,
}: {
  page: Page;
  snapshot: DashboardSnapshot;
  taskStatus: ManagedTaskStatus;
  busy: boolean;
  navigate: (page: Page) => void;
  execute: (action: () => Promise<unknown>, message?: string) => Promise<void>;
  selectProfile: (id: string) => Promise<void>;
  startManagedTask: (input: StartManagedTaskInput) => Promise<void>;
  requestCancelManagedTask: () => void;
  requestDelete: (title: string, detail: string, action: () => Promise<void>) => void;
  notify: (message: string) => void;
  workspaceSettings: DesktopWorkspaceSettings;
  workspaceHistory: DesktopWorkspaceHistoryItem[];
  onChangeWorkspaceMode: (mode: DesktopWorkspaceMode) => Promise<void>;
  onRestoreWorkspace: (id: string) => Promise<void>;
}) {
  if (page === "dashboard")
    return (
      <Dashboard
        snapshot={snapshot}
        taskStatus={taskStatus}
        busy={busy}
        onNavigate={navigate}
        onStartTask={startManagedTask}
        onRequestCancelTask={requestCancelManagedTask}
        workspaceMode={workspaceSettings.mode}
      />
    );
  if (page === "profiles")
    return (
      <Profiles
        profiles={snapshot.profiles}
        busy={busy}
        onSelect={selectProfile}
        onStartOAuth={api.startOAuthImport}
        onOAuthStatus={api.oauthImportStatus}
        onCancelOAuth={api.cancelOAuthImport}
        onCompleteOAuth={async (attemptId, alias) =>
          execute(
            () => api.completeOAuthImport(attemptId, alias),
            alias ? "档案已创建，OAuth 凭据已保存。" : "档案凭据已更新。",
          )
        }
        onSyncAccount={async (id) => {
          await execute(() => api.syncProfileAccountInfo(id), "账号资料已同步。");
        }}
        onDelete={(id, alias) =>
          requestDelete(
            `删除“${alias}”？`,
            "凭据会从系统安全存储中删除；此操作不可撤销。",
            () => api.deleteProfile(id),
          )
        }
        workspaceMode={workspaceSettings.mode}
      />
    );
  if (page === "gateway")
    return (
      <Gateway
        gateway={snapshot.gateway}
        busy={busy}
        onSave={async (input) =>
          execute(() => api.updateGateway(input), "网关配置已保存。")
        }
        onStart={async () => execute(api.startGateway, "网关已启动并使用 HTTPS 保护。")}
        onStop={async () => execute(api.stopGateway, "网关已停止。")}
        onNotice={notify}
      />
    );
  if (page === "settings")
    return (
      <Settings
        settings={workspaceSettings}
        workspaces={workspaceHistory}
        busy={busy}
        onChangeMode={onChangeWorkspaceMode}
        onRestore={onRestoreWorkspace}
        onDelete={(id, alias) =>
          requestDelete(
            `删除“${alias}”的全新工作区？`,
            "该工作区的本地客户端数据会被永久删除，无法恢复。",
            () => api.deleteDesktopWorkspace(id),
          )
        }
      />
    );
  return (
    <Notifications
      channels={snapshot.notifications}
      busy={busy}
      onSave={async (input) =>
        execute(() => api.upsertChannel(input), "通知频道已安全保存。")
      }
      onTest={async (id) => execute(() => api.testChannel(id), "测试投递已完成。")}
      onDelete={(id, name) =>
        requestDelete(
          `删除“${name}”？`,
          "频道 URL 与签名密钥会从系统安全存储中删除。",
          () => api.deleteChannel(id),
        )
      }
    />
  );
}

function NavItem({
  active,
  icon,
  label,
  onClick,
}: {
  active: boolean;
  icon: ReactNode;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      className={`nav-item ${active ? "active" : ""}`}
      type="button"
      onClick={onClick}
    >
      {icon}
      <span>{label}</span>
    </button>
  );
}
function LoadingState({
  error,
  retry,
}: {
  error: string | null;
  retry: () => Promise<void>;
}) {
  return (
    <div className="loading-state">
      <div className="loading-mark">
        <Lightning size={26} weight="fill" />
      </div>
      <h1>{error ? "本机核心暂不可用" : "正在连接安全本机核心…"}</h1>
      <p>{error ?? "只会读取经过掩码处理的状态与聚合指标。"}</p>
      {error && (
        <button className="primary-button" type="button" onClick={() => void retry()}>
          重新连接
        </button>
      )}
    </div>
  );
}
function ConfirmDialog({
  confirmation,
  busy,
  close,
  execute,
}: {
  confirmation: Exclude<Confirmation, null>;
  busy: boolean;
  close: () => void;
  execute: (action: () => Promise<unknown>, message?: string) => Promise<void>;
}) {
  return (
    <div className="dialog-backdrop" role="presentation">
      <section
        className="confirm-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
      >
        <p className="section-kicker">Confirmation required</p>
        <h2 id="confirm-title">{confirmation.title}</h2>
        <p>{confirmation.detail}</p>
        <div>
          <button className="quiet-button" type="button" onClick={close}>
            取消
          </button>
          <button
            className="danger-button"
            disabled={busy}
            type="button"
            onClick={() => {
              void execute(confirmation.action, confirmation.successMessage);
              close();
            }}
          >
            {confirmation.confirmLabel}
          </button>
        </div>
      </section>
    </div>
  );
}

function ProfileActivationDialog({
  activation,
}: {
  activation: CurrentProfileActivation;
}) {
  return (
    <div className="dialog-backdrop" role="presentation">
      <section
        className="confirm-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="profile-activation-title"
      >
        <p className="section-kicker">Switching account</p>
        <h2 id="profile-activation-title">正在切换已保存的账号</h2>
        <p>{activation.message}</p>
        <p>无需重新 OAuth。ChatGPT Chat/Work 的独立登录会话不会被读取、写入或切换。</p>
      </section>
    </div>
  );
}

function errorMessage(reason: unknown) {
  return reason instanceof RelayError
    ? reason.message
    : "发生了安全错误，请检查本机服务后重试。";
}

export default App;
