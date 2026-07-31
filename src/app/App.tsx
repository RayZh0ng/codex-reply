import { ArrowsClockwise } from "@phosphor-icons/react/ArrowsClockwise";
import { ChartPieSlice } from "@phosphor-icons/react/ChartPieSlice";
import { ChatCircleDots } from "@phosphor-icons/react/ChatCircleDots";
import { ClockCounterClockwise } from "@phosphor-icons/react/ClockCounterClockwise";
import { GearSix } from "@phosphor-icons/react/GearSix";
import { Lightning } from "@phosphor-icons/react/Lightning";
import { List } from "@phosphor-icons/react/List";
import { ShieldWarning } from "@phosphor-icons/react/ShieldWarning";
import { UsersThree } from "@phosphor-icons/react/UsersThree";
import {
  lazy,
  Suspense,
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { flushSync } from "react-dom";
import { listen } from "@tauri-apps/api/event";

import { Dashboard } from "../features/dashboard/Dashboard";
import {
  APP_UPDATE_PROGRESS_EVENT,
  CODEX_HISTORY_SYNC_FINISHED_EVENT,
  api,
  type AppUpdateChannel,
  type AppUpdateInfo,
  type AppUpdateProgressEvent,
  type AppUpdateSettings,
  type CurrentProfileActivation,
  type CodexEnvironmentInstallReport,
  type CodexEnvironmentReport,
  type CodexHistoryTransitionStatus,
  type DesktopWorkspaceHistoryItem,
  type CodexSessionSummary,
  type CollaborationContextSummary,
  type CollaborationProjectBinding,
  type DashboardSnapshot,
  type ManagedTaskStatus,
  type MaskedCollaborationBot,
  type MaskedProfile,
  type StartManagedTaskInput,
  RelayError,
} from "../shared/ipc";
import { Button, Dialog, StatusPill } from "../shared/ui";
import { useTheme } from "../shared/theme";
import type { ThemePreference } from "../shared/theme";
import logo from "../../src-tauri/icons/icon.png";

const loadProfilesPage = () => import("../features/profiles/Profiles");
const loadGatewayPage = () => import("../features/gateway/Gateway");
const loadSessionsPage = () => import("../features/sessions/Sessions");
const loadCollaborationPage = () => import("../features/collaboration/Collaboration");
const loadSettingsPage = () => import("../features/settings/Settings");

const Profiles = lazy(() =>
  loadProfilesPage().then((module) => ({ default: module.Profiles })),
);
const Gateway = lazy(() =>
  loadGatewayPage().then((module) => ({ default: module.Gateway })),
);
const Sessions = lazy(() =>
  loadSessionsPage().then((module) => ({ default: module.Sessions })),
);
const Collaboration = lazy(() =>
  loadCollaborationPage().then((module) => ({ default: module.Collaboration })),
);
const Settings = lazy(() =>
  loadSettingsPage().then((module) => ({ default: module.Settings })),
);

type Page =
  "dashboard" | "profiles" | "gateway" | "sessions" | "collaboration" | "settings";
type Confirmation = {
  title: string;
  detail: string;
  confirmLabel: string;
  successMessage?: string;
  action: () => Promise<void>;
  refresh?: () => Promise<void>;
} | null;

const idleTaskStatus: ManagedTaskStatus = {
  phase: "idle",
  profile_id: null,
  message: "当前没有正在运行的受管 Codex 任务。",
};
const PROFILE_ACTIVATION_TIMEOUT_MS = 20_000;
const defaultAppUpdateSettings: AppUpdateSettings = {
  channel: "stable",
  auto_check: true,
};
const COMPACT_SIDEBAR_QUERY = "(max-width: 1179px)";
const SIDEBAR_STORAGE_KEY = "codex-relay.sidebar.v1";

function readSidebarCollapsed() {
  try {
    return window.localStorage.getItem(SIDEBAR_STORAGE_KEY) === "collapsed";
  } catch {
    return false;
  }
}

function App() {
  const contentScroll = useRef<HTMLDivElement>(null);
  const sidebarToggle = useRef<HTMLButtonElement>(null);
  const quotaRefreshInFlight = useRef(false);
  const hasRefreshableCodexProfiles = useRef(false);
  const previousTaskPhase = useRef<ManagedTaskStatus["phase"]>(idleTaskStatus.phase);
  const [page, setPage] = useState<Page>("dashboard");
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [collaborationBots, setCollaborationBots] = useState<MaskedCollaborationBot[]>(
    [],
  );
  const [collaborationBindings, setCollaborationBindings] = useState<
    CollaborationProjectBinding[]
  >([]);
  const [codexSessions, setCodexSessions] = useState<CodexSessionSummary[]>([]);
  const [collaborationContexts, setCollaborationContexts] = useState<
    CollaborationContextSummary[]
  >([]);
  const [gatewayModelOptions, setGatewayModelOptions] = useState<string[]>([]);
  const [taskStatus, setTaskStatus] = useState<ManagedTaskStatus>(idleTaskStatus);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionBusy, setActionBusy] = useState(false);
  const [confirmation, setConfirmation] = useState<Confirmation>(null);
  const [profileActivation, setProfileActivation] =
    useState<CurrentProfileActivation | null>(null);
  const [historySyncStatus, setHistorySyncStatus] =
    useState<CodexHistoryTransitionStatus | null>(null);
  const [appUpdateSettings, setAppUpdateSettings] = useState<AppUpdateSettings>(
    defaultAppUpdateSettings,
  );
  const [availableAppUpdate, setAvailableAppUpdate] = useState<AppUpdateInfo | null>(
    null,
  );
  const [appUpdateStatus, setAppUpdateStatus] = useState<string | null>(null);
  const [appUpdateBusy, setAppUpdateBusy] = useState(false);
  const [appUpdateProgress, setAppUpdateProgress] =
    useState<AppUpdateProgressEvent | null>(null);
  const [workspaceHistory, setWorkspaceHistory] = useState<
    DesktopWorkspaceHistoryItem[]
  >([]);
  const [codexEnvironment, setCodexEnvironment] =
    useState<CodexEnvironmentReport | null>(null);
  const [codexEnvironmentInstall, setCodexEnvironmentInstall] =
    useState<CodexEnvironmentInstallReport | null>(null);
  const [codexEnvironmentBusy, setCodexEnvironmentBusy] = useState(false);
  const [topbarScrolled, setTopbarScrolled] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(readSidebarCollapsed);
  const [compactSidebar, setCompactSidebar] = useState(
    () => window.matchMedia(COMPACT_SIDEBAR_QUERY).matches,
  );
  const [sidebarDrawerOpen, setSidebarDrawerOpen] = useState(false);
  const { preference: themePreference, setPreference: setThemePreference } = useTheme();

  const busy = actionBusy || profileActivation?.status === "switching";
  const refresh = useCallback(async () => {
    try {
      const [nextSnapshot, nextTaskStatus] = await Promise.all([
        api.dashboard(),
        api.managedTaskStatus(),
      ]);
      setSnapshot(nextSnapshot);
      setTaskStatus(nextTaskStatus);
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);
  const refreshCollaboration = useCallback(async () => {
    try {
      const [bots, bindings, sessions, contexts, models] = await Promise.all([
        api.listCollaborationBots(),
        api.listCollaborationProjectBindings(),
        api.listCodexSessions(),
        api.listCollaborationContexts(),
        api.listGatewayModelOptions(),
      ]);
      setCollaborationBots(bots);
      setCollaborationBindings(bindings);
      setCodexSessions(sessions);
      setCollaborationContexts(contexts);
      setGatewayModelOptions(models);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);
  const refreshWorkspaceHistory = useCallback(async () => {
    try {
      const [history, environment] = await Promise.all([
        api.listDesktopWorkspaces(),
        api.codexEnvironmentStatus(),
      ]);
      setWorkspaceHistory(history);
      setCodexEnvironment(environment);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);
  const installCodexEnvironment = useCallback(async () => {
    setCodexEnvironmentBusy(true);
    try {
      const report = await api.installCodexEnvironment();
      setCodexEnvironmentInstall(report);
      setCodexEnvironment(report.environment);
      setNotice(report.message);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setCodexEnvironmentBusy(false);
    }
  }, []);
  const installAppUpdate = useCallback(async (update: AppUpdateInfo) => {
    setAppUpdateBusy(true);
    setAppUpdateStatus(null);
    setAppUpdateProgress({
      phase: "checking",
      channel: update.channel,
      version: update.version,
      current_version: update.current_version,
      downloaded_bytes: 0,
      content_length: null,
      progress_percent: null,
      message: "正在准备下载更新。",
      updated_at_ms: Date.now(),
    });
    try {
      await api.installAppUpdate(update.channel);
      setAppUpdateProgress((current) =>
        current?.phase === "restarting"
          ? current
          : {
              phase: "restarting",
              channel: update.channel,
              version: update.version,
              current_version: update.current_version,
              downloaded_bytes: current?.downloaded_bytes ?? 0,
              content_length: current?.content_length ?? null,
              progress_percent: 100,
              message: "更新已安装，应用即将重启。",
              updated_at_ms: Date.now(),
            },
      );
      setNotice("更新已安装，应用将重启。");
    } catch (reason) {
      const message = errorMessage(reason);
      setAppUpdateStatus(message);
      setAppUpdateProgress((current) => ({
        phase: "failed",
        channel: update.channel,
        version: update.version,
        current_version: update.current_version,
        downloaded_bytes: current?.downloaded_bytes ?? 0,
        content_length: current?.content_length ?? null,
        progress_percent: current?.progress_percent ?? null,
        message,
        updated_at_ms: Date.now(),
      }));
      throw reason;
    } finally {
      setAppUpdateBusy(false);
    }
  }, []);
  const requestAppUpdateInstall = useCallback(
    (update: AppUpdateInfo) => {
      setConfirmation({
        title: `安装 Codex Relay ${update.version}？`,
        detail:
          `当前版本 ${update.current_version}，将从 ${formatAppUpdateChannel(update.channel)} 通道下载安装包。` +
          "安装完成后应用会重启，Windows 可能会在安装阶段自动退出。",
        confirmLabel: "安装并重启",
        successMessage: "正在安装更新，应用将重启。",
        action: () => installAppUpdate(update),
        refresh: async () => {},
      });
    },
    [installAppUpdate],
  );
  const checkForAppUpdate = useCallback(
    async (channel: AppUpdateChannel, prompt = false, showStatus = true) => {
      setAppUpdateBusy(true);
      setAppUpdateProgress(null);
      try {
        const update = await api.checkAppUpdate(channel);
        setAvailableAppUpdate(update);
        setAppUpdateStatus(update ? null : "当前已是最新版本。");
        if (update && prompt) requestAppUpdateInstall(update);
        return update;
      } catch (reason) {
        if (showStatus) setAppUpdateStatus(errorMessage(reason));
        return null;
      } finally {
        setAppUpdateBusy(false);
      }
    },
    [requestAppUpdateInstall],
  );
  const changeAppUpdateSettings = useCallback(async (settings: AppUpdateSettings) => {
    setAppUpdateBusy(true);
    setAppUpdateProgress(null);
    try {
      const saved = await api.updateAppUpdateSettings(settings);
      setAppUpdateSettings(saved);
      setAvailableAppUpdate(null);
      setAppUpdateStatus("软件更新设置已保存。");
    } catch (reason) {
      setAppUpdateStatus(errorMessage(reason));
    } finally {
      setAppUpdateBusy(false);
    }
  }, []);
  hasRefreshableCodexProfiles.current =
    snapshot?.profiles.some(
      (profile) => profile.kind === "codex_oauth" && profile.credential_configured,
    ) ?? false;
  const refreshQuotaSummaries = useCallback(
    async (force = false) => {
      if (
        quotaRefreshInFlight.current ||
        !hasRefreshableCodexProfiles.current ||
        (!force && (page !== "profiles" || document.visibilityState !== "visible")) ||
        navigator.onLine === false
      )
        return;
      quotaRefreshInFlight.current = true;
      try {
        await api.refreshProfileQuotas();
        await refresh();
      } catch {
        // Per-profile cached states carry refresh failures; do not replace the whole app with an error.
      } finally {
        quotaRefreshInFlight.current = false;
      }
    },
    [page, refresh],
  );
  useEffect(() => {
    void refresh();
  }, [refresh]);
  useEffect(() => {
    let cancelled = false;
    void api
      .appUpdateSettings()
      .then(async (settings) => {
        if (cancelled) return;
        setAppUpdateSettings(settings);
        if (settings.auto_check && navigator.onLine !== false) {
          await checkForAppUpdate(settings.channel, true, false);
        }
      })
      .catch(() => {
        // Startup update checks must not block the main local-first experience.
      });
    return () => {
      cancelled = true;
    };
  }, [checkForAppUpdate]);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let mounted = true;
    void listen<AppUpdateProgressEvent>(APP_UPDATE_PROGRESS_EVENT, (event) => {
      if (!mounted) return;
      setAppUpdateProgress(event.payload);
      if (event.payload.phase === "failed") {
        setAppUpdateStatus(event.payload.message);
        setAppUpdateBusy(false);
      } else {
        setAppUpdateStatus(null);
      }
    })
      .then((nextUnlisten) => {
        unlisten = nextUnlisten;
        if (!mounted) unlisten();
      })
      .catch(() => {
        // Progress events are available only inside the Tauri runtime.
      });
    return () => {
      mounted = false;
      unlisten?.();
    };
  }, []);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let mounted = true;
    void listen<CodexHistoryTransitionStatus>(
      CODEX_HISTORY_SYNC_FINISHED_EVENT,
      (event) => {
        if (!mounted) return;
        setHistorySyncStatus(event.payload);
        setNotice(event.payload.message);
      },
    )
      .then((nextUnlisten) => {
        unlisten = nextUnlisten;
        if (!mounted) unlisten();
      })
      .catch(() => {
        // History transition events are available only inside the Tauri runtime.
      });
    return () => {
      mounted = false;
      unlisten?.();
    };
  }, []);
  useEffect(() => {
    const media = window.matchMedia(COMPACT_SIDEBAR_QUERY);
    const update = (event: MediaQueryListEvent | MediaQueryList) => {
      setCompactSidebar(event.matches);
      if (!event.matches) setSidebarDrawerOpen(false);
    };
    update(media);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  useEffect(() => {
    if (!sidebarDrawerOpen) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setSidebarDrawerOpen(false);
      sidebarToggle.current?.focus();
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [sidebarDrawerOpen]);
  useEffect(() => {
    if (page === "collaboration") void refreshCollaboration();
    if (page === "settings") void refreshWorkspaceHistory();
  }, [page, refreshCollaboration, refreshWorkspaceHistory]);
  useEffect(() => {
    if (page === "profiles") void refreshQuotaSummaries();
  }, [page, refreshQuotaSummaries]);
  useEffect(() => {
    const refreshOnFocus = () => void refreshQuotaSummaries();
    const refreshWhenVisible = () => {
      if (document.visibilityState === "visible") void refreshQuotaSummaries();
    };
    window.addEventListener("focus", refreshOnFocus);
    document.addEventListener("visibilitychange", refreshWhenVisible);
    const timer = window.setInterval(() => void refreshQuotaSummaries(), 30_000);
    return () => {
      window.removeEventListener("focus", refreshOnFocus);
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
    const previousPhase = previousTaskPhase.current;
    previousTaskPhase.current = taskStatus.phase;
    if (previousPhase === "running" && taskStatus.phase !== "running") {
      void refreshQuotaSummaries(true);
    }
  }, [refreshQuotaSummaries, taskStatus.phase]);
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(timer);
  }, [notice]);
  const execute = async (
    action: () => Promise<unknown>,
    message?: string,
    refreshAction?: () => Promise<void>,
  ) => {
    setActionBusy(true);
    try {
      await action();
      await (refreshAction ?? refreshCurrentPage)();
      if (message) setNotice(message);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setActionBusy(false);
    }
  };
  const syncProfileAccount = async (id: string) => {
    try {
      const profile = await api.syncProfileAccountInfo(id);
      await refresh();
      const quota = profile.account?.quota;
      setNotice(
        quota?.status === "available"
          ? "账号资料与额度已刷新。"
          : quota?.message || "账号资料已刷新，但上游暂未返回额度。",
      );
      return profile;
    } catch (reason) {
      setError(errorMessage(reason));
      throw reason;
    }
  };
  const selectProfileDirect = async (id: string, confirmedDesktopRestart = false) => {
    setActionBusy(true);
    try {
      const activation = await api.selectProfile(id, confirmedDesktopRestart);
      if (activation.history_sync_status) {
        setHistorySyncStatus(activation.history_sync_status);
      }
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
    setConfirmation({
      title: "关闭并切换 Codex 客户端？",
      detail:
        "切换账号会先请求正常退出 ChatGPT/Codex，再复用原客户端数据目录启动。已保存的聊天记录、记忆、设置与状态会保留，但未发送内容可能丢失。",
      confirmLabel: "关闭并切换",
      action: () => selectProfileDirect(id, true),
    });
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
          if (next.history_sync_status) setHistorySyncStatus(next.history_sync_status);
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
  const requestDelete = (
    title: string,
    detail: string,
    action: () => Promise<void>,
    refreshAction?: () => Promise<void>,
  ) =>
    setConfirmation({
      title,
      detail,
      confirmLabel: "确认删除",
      successMessage: "操作已完成。",
      action,
      refresh: refreshAction,
    });
  const navigate = useCallback(
    (nextPage: Page) => {
      const updatePage = () => {
        flushSync(() => setPage(nextPage));
        setTopbarScrolled(false);
        const scrollContainer = contentScroll.current;
        if (typeof scrollContainer?.scrollTo === "function") {
          scrollContainer.scrollTo({ top: 0 });
        } else if (scrollContainer) {
          scrollContainer.scrollTop = 0;
        }
      };
      const reduceMotion = window.matchMedia(
        "(prefers-reduced-motion: reduce)",
      ).matches;
      if (!reduceMotion && typeof document.startViewTransition === "function") {
        document.startViewTransition(updatePage);
      } else {
        updatePage();
      }
      if (compactSidebar) setSidebarDrawerOpen(false);
    },
    [compactSidebar],
  );
  const refreshCurrentPage = useCallback(async () => {
    setRefreshing(true);
    try {
      await refresh();
      if (page === "collaboration") await refreshCollaboration();
      if (page === "settings") await refreshWorkspaceHistory();
    } finally {
      setRefreshing(false);
    }
  }, [page, refresh, refreshCollaboration, refreshWorkspaceHistory]);
  const appUpdateProgressActive = Boolean(
    appUpdateProgress && appUpdateProgress.phase !== "failed",
  );
  const appUpdateWorkBusy = appUpdateBusy || appUpdateProgressActive;
  const sidebarExpanded = compactSidebar ? sidebarDrawerOpen : !sidebarCollapsed;
  const toggleSidebar = () => {
    if (compactSidebar) {
      setSidebarDrawerOpen((open) => !open);
      return;
    }
    setSidebarCollapsed((collapsed) => {
      const next = !collapsed;
      try {
        window.localStorage.setItem(
          SIDEBAR_STORAGE_KEY,
          next ? "collapsed" : "expanded",
        );
      } catch {
        // The current window can still use the selected layout.
      }
      return next;
    });
  };
  const content = snapshot ? (
    <PageContent
      page={page}
      snapshot={snapshot}
      taskStatus={taskStatus}
      busy={busy}
      navigate={navigate}
      execute={execute}
      selectProfile={selectProfile}
      syncProfileAccount={syncProfileAccount}
      startManagedTask={startManagedTask}
      requestCancelManagedTask={requestCancelManagedTask}
      requestDelete={requestDelete}
      notify={setNotice}
      appUpdateSettings={appUpdateSettings}
      availableAppUpdate={availableAppUpdate}
      appUpdateStatus={appUpdateStatus}
      appUpdateBusy={appUpdateWorkBusy}
      appUpdateProgress={appUpdateProgress}
      workspaceHistory={workspaceHistory}
      codexEnvironment={codexEnvironment}
      codexEnvironmentInstall={codexEnvironmentInstall}
      codexEnvironmentBusy={codexEnvironmentBusy}
      themePreference={themePreference}
      collaborationBots={collaborationBots}
      collaborationBindings={collaborationBindings}
      codexSessions={codexSessions}
      collaborationContexts={collaborationContexts}
      gatewayModelOptions={gatewayModelOptions}
      historySyncStatus={historySyncStatus}
      onHistorySyncStatus={setHistorySyncStatus}
      onChangeAppUpdateSettings={changeAppUpdateSettings}
      onCheckAppUpdate={async () => {
        await checkForAppUpdate(appUpdateSettings.channel, false, true);
      }}
      onInstallAppUpdate={async () => {
        if (availableAppUpdate) requestAppUpdateInstall(availableAppUpdate);
      }}
      onRefreshCodexEnvironment={async () => {
        setCodexEnvironmentBusy(true);
        try {
          setCodexEnvironment(await api.codexEnvironmentStatus());
        } finally {
          setCodexEnvironmentBusy(false);
        }
      }}
      onInstallCodexEnvironment={installCodexEnvironment}
      onThemePreferenceChange={setThemePreference}
      onRefresh={refreshCurrentPage}
      onRefreshCollaboration={refreshCollaboration}
      onJsonImportComplete={refresh}
    />
  ) : (
    <LoadingState error={error} retry={refresh} />
  );

  return (
    <main
      className={`app-shell ${sidebarCollapsed ? "is-sidebar-collapsed" : ""} ${sidebarDrawerOpen ? "is-sidebar-drawer-open" : ""}`}
      data-page={page}
    >
      <button
        aria-hidden={!sidebarDrawerOpen}
        aria-label="关闭侧边栏"
        className="sidebar-scrim"
        onClick={() => {
          setSidebarDrawerOpen(false);
          sidebarToggle.current?.focus();
        }}
        tabIndex={sidebarDrawerOpen ? 0 : -1}
        type="button"
      />
      <aside
        aria-label="应用侧边栏"
        className="sidebar"
        data-expanded={sidebarExpanded}
        data-tauri-drag-region
        id="app-sidebar"
      >
        <div className="brand" data-tauri-drag-region>
          <img src={logo} alt="" />
          <span>Codex Relay</span>
        </div>
        <nav aria-label="主导航">
          <NavItem
            active={page === "dashboard"}
            icon={<ChartPieSlice size={20} />}
            label="总览"
            onClick={() => navigate("dashboard")}
          />
          <NavItem
            active={page === "profiles"}
            icon={<UsersThree size={20} />}
            label="档案"
            onClick={() => navigate("profiles")}
            prefetch={loadProfilesPage}
          />
          <NavItem
            active={page === "gateway"}
            icon={<Lightning size={20} />}
            label="网关"
            onClick={() => navigate("gateway")}
            prefetch={loadGatewayPage}
          />
          <NavItem
            active={page === "sessions"}
            icon={<ClockCounterClockwise size={20} />}
            label="会话"
            onClick={() => navigate("sessions")}
            prefetch={loadSessionsPage}
          />
          <NavItem
            active={page === "collaboration"}
            icon={<ChatCircleDots size={20} />}
            label="协作"
            onClick={() => navigate("collaboration")}
            prefetch={loadCollaborationPage}
          />
        </nav>
        <div className="sidebar-bottom">
          <NavItem
            active={page === "settings"}
            icon={<GearSix size={20} />}
            label="设置"
            onClick={() => navigate("settings")}
            prefetch={loadSettingsPage}
          />
        </div>
      </aside>
      <section className="app-main">
        <header className={`topbar ${topbarScrolled ? "is-scrolled" : ""}`}>
          <div
            aria-hidden="true"
            className="topbar-drag-region"
            data-tauri-drag-region
          />
          <div className="crumb">
            <button
              aria-controls="app-sidebar"
              aria-expanded={sidebarExpanded}
              aria-label={sidebarExpanded ? "收起侧边栏" : "展开侧边栏"}
              className="sidebar-toggle"
              onClick={toggleSidebar}
              ref={sidebarToggle}
              title={sidebarExpanded ? "收起侧边栏" : "展开侧边栏"}
              type="button"
            >
              <List size={19} />
            </button>
            <span>{pageLabel(page)}</span>
          </div>
          <div className="topbar-status">
            {snapshot && (
              <>
                <StatusPill
                  compact
                  tone={snapshot.gateway.running ? "running" : "disabled"}
                >
                  {snapshot.gateway.running ? "服务运行中" : "服务未启动"}
                </StatusPill>
                <Button
                  aria-label="刷新状态"
                  className={`topbar-refresh ${refreshing ? "is-refreshing" : ""}`}
                  disabled={refreshing}
                  onClick={() => void refreshCurrentPage()}
                  size="sm"
                  title="刷新状态"
                  variant="icon"
                >
                  <ArrowsClockwise size={17} />
                </Button>
              </>
            )}
          </div>
        </header>
        <div
          className="content-scroll"
          onScroll={(event) => setTopbarScrolled(event.currentTarget.scrollTop > 6)}
          ref={contentScroll}
        >
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
                  void refreshCurrentPage();
                }}
              >
                重试
              </button>
            </div>
          )}
          <Suspense fallback={<PageSkeleton />}>
            <div className="page-stage" key={page}>
              {content}
            </div>
          </Suspense>
        </div>
        {notice && (
          <div className="toast" role="status">
            {notice}
          </div>
        )}
      </section>
      {confirmation && (
        <ConfirmDialog
          confirmation={confirmation}
          busy={busy}
          close={() => setConfirmation(null)}
          execute={execute}
        />
      )}
      {appUpdateProgress && (
        <AppUpdateProgressDialog
          progress={appUpdateProgress}
          onClose={() => setAppUpdateProgress(null)}
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
  syncProfileAccount,
  startManagedTask,
  requestCancelManagedTask,
  requestDelete,
  notify,
  appUpdateSettings,
  availableAppUpdate,
  appUpdateStatus,
  appUpdateBusy,
  appUpdateProgress,
  workspaceHistory,
  codexEnvironment,
  codexEnvironmentInstall,
  codexEnvironmentBusy,
  themePreference,
  collaborationBots,
  collaborationBindings,
  codexSessions,
  collaborationContexts,
  gatewayModelOptions,
  historySyncStatus,
  onHistorySyncStatus,
  onChangeAppUpdateSettings,
  onCheckAppUpdate,
  onInstallAppUpdate,
  onRefreshCodexEnvironment,
  onInstallCodexEnvironment,
  onThemePreferenceChange,
  onRefresh,
  onRefreshCollaboration,
  onJsonImportComplete,
}: {
  page: Page;
  snapshot: DashboardSnapshot;
  taskStatus: ManagedTaskStatus;
  busy: boolean;
  navigate: (page: Page) => void;
  execute: (
    action: () => Promise<unknown>,
    message?: string,
    refreshAction?: () => Promise<void>,
  ) => Promise<void>;
  selectProfile: (id: string) => Promise<void>;
  syncProfileAccount: (id: string) => Promise<MaskedProfile>;
  startManagedTask: (input: StartManagedTaskInput) => Promise<void>;
  requestCancelManagedTask: () => void;
  requestDelete: (
    title: string,
    detail: string,
    action: () => Promise<void>,
    refreshAction?: () => Promise<void>,
  ) => void;
  notify: (message: string) => void;
  appUpdateSettings: AppUpdateSettings;
  availableAppUpdate: AppUpdateInfo | null;
  appUpdateStatus: string | null;
  appUpdateBusy: boolean;
  appUpdateProgress: AppUpdateProgressEvent | null;
  workspaceHistory: DesktopWorkspaceHistoryItem[];
  codexEnvironment: CodexEnvironmentReport | null;
  codexEnvironmentInstall: CodexEnvironmentInstallReport | null;
  codexEnvironmentBusy: boolean;
  themePreference: ThemePreference;
  collaborationBots: MaskedCollaborationBot[];
  collaborationBindings: CollaborationProjectBinding[];
  codexSessions: CodexSessionSummary[];
  collaborationContexts: CollaborationContextSummary[];
  gatewayModelOptions: string[];
  historySyncStatus: CodexHistoryTransitionStatus | null;
  onHistorySyncStatus: (status: CodexHistoryTransitionStatus) => void;
  onChangeAppUpdateSettings: (settings: AppUpdateSettings) => Promise<void>;
  onCheckAppUpdate: () => Promise<void>;
  onInstallAppUpdate: () => Promise<void>;
  onRefreshCodexEnvironment: () => Promise<void>;
  onInstallCodexEnvironment: () => Promise<void>;
  onThemePreferenceChange: (preference: ThemePreference) => void;
  onRefresh: () => Promise<void>;
  onRefreshCollaboration: () => Promise<void>;
  onJsonImportComplete: () => Promise<void>;
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
            async () => {
              const profile = await api.completeOAuthImport(attemptId, alias);
              try {
                await api.syncProfileAccountInfo(profile.id);
                await api.refreshProfileModels(profile.id);
              } catch {
                // 导入已完成；资料或模型刷新失败时保留档案，并允许用户手动刷新。
              }
            },
            alias
              ? "档案已创建，OAuth 凭据已保存，正在刷新资料与模型。"
              : "档案凭据已更新，正在刷新资料与模型。",
          )
        }
        onSyncAccount={syncProfileAccount}
        onRefreshModels={(id) =>
          execute(() => api.refreshProfileModels(id), "可用模型已从上游刷新。")
        }
        onCreateApiProfile={(input) =>
          api.createApiServiceProfile(input).then(async () => {
            await onRefresh();
            notify("第三方模型提供商已保存，已保留你选择的模型映射。");
          })
        }
        onUpdateApiProfile={(input) =>
          api.updateProfile(input).then(async () => {
            await onRefresh();
            await onRefreshCollaboration();
            notify("第三方模型提供商已更新。");
          })
        }
        onTogglePool={(profile) =>
          execute(
            async () => {
              const models =
                !profile.in_pool && !profile.models.length
                  ? (await api.refreshProfileModels(profile.id)).models
                  : profile.models;
              await api.updateProfile({
                id: profile.id,
                alias: profile.alias,
                enabled: profile.enabled,
                in_pool: !profile.in_pool,
                priority: profile.priority,
                weight: profile.weight,
                models,
                api_key: null,
              });
            },
            profile.in_pool ? "已移出网关账号池。" : "已加入网关账号池。",
            async () => {
              await onRefresh();
              await onRefreshCollaboration();
            },
          )
        }
        onConfigurePool={(profile, priority, weight, models) =>
          execute(
            () =>
              api.updateProfile({
                id: profile.id,
                alias: profile.alias,
                enabled: profile.enabled,
                in_pool: profile.in_pool,
                priority,
                weight,
                models,
                api_key: null,
              }),
            "账号池优先级、权重与模型范围已更新。",
            async () => {
              await onRefresh();
              await onRefreshCollaboration();
            },
          )
        }
        onActivateApiProfile={(profile) =>
          execute(async () => {
            const status = await api.activateApiServiceProfile(profile.id);
            if (status.history_sync_status) {
              onHistorySyncStatus(status.history_sync_status);
            }
            notify(status.message);
          })
        }
        onDelete={(id, alias) =>
          requestDelete(
            `删除“${alias}”？`,
            "凭据会从系统安全存储中删除；此操作不可撤销。",
            () => api.deleteProfile(id),
          )
        }
        onJsonImportComplete={onJsonImportComplete}
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
        onHistorySyncStatus={onHistorySyncStatus}
        onNavigateProfiles={() => navigate("profiles")}
        onRefresh={onRefresh}
      />
    );
  if (page === "sessions")
    return (
      <Sessions busy={busy} historySyncStatus={historySyncStatus} onNotice={notify} />
    );
  if (page === "collaboration")
    return (
      <Collaboration
        bots={collaborationBots}
        bindings={collaborationBindings}
        sessions={codexSessions}
        contexts={collaborationContexts}
        profiles={snapshot.profiles}
        gatewayModelOptions={gatewayModelOptions}
        busy={busy}
        onSaveBot={async (input) =>
          execute(() => api.upsertCollaborationBot(input), "协作机器人已保存。")
        }
        onTestBot={async (id) =>
          execute(() => api.testCollaborationBot(id), "协作机器人配置已验证。")
        }
        onDeleteBot={(id, name) =>
          requestDelete(
            `删除协作机器人“${name}”？`,
            "App Secret 引用、项目绑定和会话记录会从本机删除。",
            () => api.deleteCollaborationBot(id),
          )
        }
        onSaveBinding={async (input) =>
          execute(
            () => api.upsertCollaborationProjectBinding(input),
            "项目绑定已创建。",
          )
        }
        onRegisterDiscordCommands={async (id) =>
          execute(
            () => api.registerDiscordCommands(id),
            "Discord slash command 已注册。",
          )
        }
        onLoadCallbackStatus={api.collaborationCallbackStatus}
        onDeleteBinding={(id, name) =>
          requestDelete(
            `删除项目绑定“${name}”？`,
            "该项目的群绑定和会话记录会从本机删除。",
            () => api.deleteCollaborationProjectBinding(id),
          )
        }
        onCancelSession={async (id) =>
          execute(() => api.cancelCodexSession(id), "Codex 会话已取消。")
        }
        onContinueSession={async (id, instruction) =>
          execute(() => api.continueCodexSession(id, instruction), "Codex 会话已继续。")
        }
        onUpdateContext={async (input) =>
          execute(() => api.updateCollaborationContext(input), "协作上下文已保存。")
        }
        onResetContext={async (id) =>
          execute(() => api.resetCollaborationContext(id), "协作上下文已重置。")
        }
      />
    );
  if (page === "settings")
    return (
      <Settings
        workspaces={workspaceHistory}
        codexEnvironment={codexEnvironment}
        codexEnvironmentInstall={codexEnvironmentInstall}
        codexEnvironmentBusy={codexEnvironmentBusy}
        busy={busy}
        themePreference={themePreference}
        updateSettings={appUpdateSettings}
        availableUpdate={availableAppUpdate}
        updateStatus={appUpdateStatus}
        updateBusy={appUpdateBusy}
        updateProgress={appUpdateProgress}
        onThemePreferenceChange={onThemePreferenceChange}
        onChangeUpdateSettings={onChangeAppUpdateSettings}
        onCheckUpdate={onCheckAppUpdate}
        onInstallUpdate={onInstallAppUpdate}
        onRefreshCodexEnvironment={onRefreshCodexEnvironment}
        onInstallCodexEnvironment={onInstallCodexEnvironment}
        onDelete={(id, alias) =>
          requestDelete(
            `删除“${alias}”的旧独立工作区？`,
            "该工作区的本地客户端数据会被永久删除，无法恢复。",
            () => api.deleteDesktopWorkspace(id),
            onRefresh,
          )
        }
      />
    );
  return null;
}

function NavItem({
  active,
  icon,
  label,
  onClick,
  prefetch,
}: {
  active: boolean;
  icon: ReactNode;
  label: string;
  onClick: () => void;
  prefetch?: () => Promise<unknown>;
}) {
  return (
    <button
      aria-current={active ? "page" : undefined}
      className={`nav-item ${active ? "active" : ""}`}
      type="button"
      onClick={onClick}
      onFocus={() => void prefetch?.()}
      onMouseEnter={() => void prefetch?.()}
      title={label}
    >
      <span className="nav-icon" aria-hidden="true">
        {icon}
      </span>
      <span>{label}</span>
    </button>
  );
}

function PageSkeleton() {
  return (
    <div className="page page-skeleton" aria-label="正在加载页面" role="status">
      <div className="skeleton skeleton-heading" />
      <div className="skeleton skeleton-subtitle" />
      <div className="skeleton-grid">
        <div className="skeleton skeleton-card" />
        <div className="skeleton skeleton-card" />
        <div className="skeleton skeleton-card" />
      </div>
    </div>
  );
}

function pageLabel(page: Page) {
  return {
    dashboard: "总览",
    profiles: "档案",
    gateway: "网关",
    sessions: "会话",
    collaboration: "协作",
    settings: "设置",
  }[page];
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
  execute: (
    action: () => Promise<unknown>,
    message?: string,
    refreshAction?: () => Promise<void>,
  ) => Promise<void>;
}) {
  return (
    <Dialog
      description={confirmation.detail}
      footer={
        <>
          <Button onClick={close} variant="quiet">
            取消
          </Button>
          <Button
            disabled={busy}
            onClick={() => {
              void execute(
                confirmation.action,
                confirmation.successMessage,
                confirmation.refresh,
              );
              close();
            }}
            variant="danger"
          >
            {confirmation.confirmLabel}
          </Button>
        </>
      }
      onClose={busy ? undefined : close}
      open
      title={confirmation.title}
    />
  );
}

function AppUpdateProgressDialog({
  progress,
  onClose,
}: {
  progress: AppUpdateProgressEvent;
  onClose: () => void;
}) {
  const failed = progress.phase === "failed";
  return (
    <Dialog
      description={progress.message}
      footer={
        failed ? (
          <Button onClick={onClose} variant="quiet">
            关闭
          </Button>
        ) : undefined
      }
      onClose={failed ? onClose : undefined}
      open
      title={`正在更新到 Codex Relay ${progress.version}`}
    >
      <div className={`update-progress-dialog is-${progress.phase}`}>
        <UpdateProgressMeter progress={progress} />
        <p>
          {formatAppUpdateChannel(progress.channel)} 通道 · 当前版本{" "}
          {progress.current_version}
        </p>
        {!failed && <p>请保持应用打开；安装完成后会自动重启。</p>}
      </div>
    </Dialog>
  );
}

function UpdateProgressMeter({ progress }: { progress: AppUpdateProgressEvent }) {
  const percent = progress.progress_percent;
  return (
    <div className="update-progress-meter" role="status">
      <div className="update-progress-meter-heading">
        <strong>{appUpdateProgressPhaseLabel(progress.phase)}</strong>
        <span>
          {percent === null ? formatBytes(progress.downloaded_bytes) : `${percent}%`}
        </span>
      </div>
      <progress
        aria-label="更新进度"
        max={100}
        value={percent === null ? undefined : percent}
      />
      <small>{formatAppUpdateByteSummary(progress)}</small>
    </div>
  );
}

function appUpdateProgressPhaseLabel(phase: AppUpdateProgressEvent["phase"]) {
  return (
    {
      checking: "准备下载",
      downloading: "正在下载",
      downloaded: "下载完成",
      installing: "正在安装",
      restarting: "准备重启",
      failed: "更新失败",
    }[phase] ?? phase
  );
}

function formatAppUpdateByteSummary(progress: AppUpdateProgressEvent) {
  if (progress.content_length && progress.content_length > 0) {
    return `${formatBytes(progress.downloaded_bytes)} / ${formatBytes(progress.content_length)}`;
  }
  if (progress.phase === "checking") return "正在连接更新服务…";
  return `${formatBytes(progress.downloaded_bytes)} 已下载`;
}

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let next = value;
  let unitIndex = 0;
  while (next >= 1024 && unitIndex < units.length - 1) {
    next /= 1024;
    unitIndex += 1;
  }
  const precision = unitIndex === 0 || Number.isInteger(next) || next >= 10 ? 0 : 1;
  return `${next.toFixed(precision)} ${units[unitIndex]}`;
}

function ProfileActivationDialog({
  activation,
}: {
  activation: CurrentProfileActivation;
}) {
  return (
    <Dialog description={activation.message} open title="正在切换已保存的账号">
      <p>无需重新 OAuth。ChatGPT Chat/Work 的独立登录会话不会被读取、写入或切换。</p>
    </Dialog>
  );
}

function formatAppUpdateChannel(channel: AppUpdateChannel) {
  return channel === "beta" ? "Beta" : "稳定版";
}

function errorMessage(reason: unknown) {
  return reason instanceof RelayError
    ? reason.message
    : "发生了安全错误，请检查本机服务后重试。";
}

export default App;
