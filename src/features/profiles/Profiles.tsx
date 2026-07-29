import { ArrowDown } from "@phosphor-icons/react/ArrowDown";
import { ArrowUp } from "@phosphor-icons/react/ArrowUp";
import { ArrowsClockwise } from "@phosphor-icons/react/ArrowsClockwise";
import { CaretDown } from "@phosphor-icons/react/CaretDown";
import { Check } from "@phosphor-icons/react/Check";
import { CheckCircle } from "@phosphor-icons/react/CheckCircle";
import { CloudArrowUp } from "@phosphor-icons/react/CloudArrowUp";
import { Key } from "@phosphor-icons/react/Key";
import { Plus } from "@phosphor-icons/react/Plus";
import { Trash } from "@phosphor-icons/react/Trash";
import { UserSwitch } from "@phosphor-icons/react/UserSwitch";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  FormEvent,
  KeyboardEvent,
  useCallback,
  useDeferredValue,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import type {
  DesktopWorkspaceMode,
  ApiServiceTestReport,
  GatewayModelMapping,
  GatewayProvider,
  GatewayWireApi,
  JsonProfileImportPreview,
  JsonProfileImportResult,
  MaskedProfile,
  OAuthImportStatus,
  ProfileQuota,
  ProfileQuotaWindow,
  ProfileSubscription,
} from "../../shared/ipc";
import { api } from "../../shared/ipc";
import { Select } from "../../shared/ui/Select";

interface ProfilesProps {
  profiles: MaskedProfile[];
  busy: boolean;
  onSelect: (id: string) => Promise<void>;
  onStartOAuth: (profileId?: string) => Promise<OAuthImportStatus>;
  onOAuthStatus: (attemptId: string) => Promise<OAuthImportStatus>;
  onCancelOAuth: (attemptId: string) => Promise<void>;
  onCompleteOAuth: (attemptId: string, alias?: string) => Promise<void>;
  onSyncAccount: (id: string) => Promise<MaskedProfile>;
  onRefreshModels?: (id: string) => Promise<void>;
  onCreateApiProfile?: (input: Record<string, unknown>) => Promise<void>;
  onTogglePool?: (profile: MaskedProfile) => Promise<void>;
  onConfigurePool?: (
    profile: MaskedProfile,
    priority: number,
    weight: number,
    models: string[],
  ) => Promise<void>;
  onActivateApiProfile?: (profile: MaskedProfile) => Promise<void>;
  onDelete: (id: string, alias: string) => void;
  onJsonImportComplete?: () => Promise<void>;
  workspaceMode?: DesktopWorkspaceMode;
}

type ImportFlow =
  | { step: "picker" }
  | { step: "authorizing"; status: OAuthImportStatus }
  | { step: "naming"; status: OAuthImportStatus }
  | { step: "json"; preview: JsonProfileImportPreview }
  | { step: "api" }
  | null;

type ProfileSortKey = "default" | "quota" | "subscription" | "reset";
type ProfileSortDirection = "urgent" | "reverse";
type OpenFilterMenu = "subscription" | "sort" | null;

interface FilterMenuOption<T extends string> {
  value: T;
  label: string;
}

const ALL_SUBSCRIPTIONS = "all";
const UNSYNCED_SUBSCRIPTION = "unsynced";
const PROFILE_SORT_OPTIONS: FilterMenuOption<ProfileSortKey>[] = [
  { value: "default", label: "默认顺序" },
  { value: "quota", label: "剩余额度" },
  { value: "subscription", label: "订阅剩余时间" },
  { value: "reset", label: "额度重置时间" },
];

export function Profiles({
  profiles,
  busy,
  onSelect,
  onStartOAuth,
  onOAuthStatus,
  onCancelOAuth,
  onCompleteOAuth,
  onSyncAccount,
  onCreateApiProfile = async () => undefined,
  onTogglePool = async () => undefined,
  onActivateApiProfile = async () => undefined,
  onDelete,
  onJsonImportComplete = async () => undefined,
  workspaceMode = "per_profile",
}: ProfilesProps) {
  const [flow, setFlow] = useState<ImportFlow>(null);
  const [nameQuery, setNameQuery] = useState("");
  const [emailQuery, setEmailQuery] = useState("");
  const deferredNameQuery = useDeferredValue(nameQuery);
  const deferredEmailQuery = useDeferredValue(emailQuery);
  const [subscriptionFilter, setSubscriptionFilter] = useState(ALL_SUBSCRIPTIONS);
  const [sortKey, setSortKey] = useState<ProfileSortKey>("default");
  const [sortDirection, setSortDirection] = useState<ProfileSortDirection>("urgent");
  const [openMenu, setOpenMenu] = useState<OpenFilterMenu>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [selectedJsonItems, setSelectedJsonItems] = useState<Set<string>>(new Set());
  const [jsonBusy, setJsonBusy] = useState(false);
  const [jsonResult, setJsonResult] = useState<JsonProfileImportResult | null>(null);
  const [refreshingProfileId, setRefreshingProfileId] = useState<string | null>(null);
  const completing = useRef(false);
  const subscriptionTypes = useMemo(
    () =>
      [
        ...new Set(
          profiles.flatMap((profile) => {
            const planType = profile.account?.subscription.plan_type;
            return planType ? [planType] : [];
          }),
        ),
      ].sort((left, right) =>
        subscriptionPlanLabel(left).localeCompare(
          subscriptionPlanLabel(right),
          "zh-CN",
        ),
      ),
    [profiles],
  );
  const subscriptionOptions = useMemo<FilterMenuOption<string>[]>(
    () => [
      { value: ALL_SUBSCRIPTIONS, label: "全部套餐" },
      ...subscriptionTypes.map((planType) => ({
        value: planType,
        label: subscriptionPlanLabel(planType),
      })),
      { value: UNSYNCED_SUBSCRIPTION, label: "尚未同步" },
    ],
    [subscriptionTypes],
  );
  const visibleProfiles = useMemo(
    () =>
      profiles
        .map((profile, index) => ({ profile, index }))
        .filter(({ profile }) =>
          matchesProfileFilters(
            profile,
            deferredNameQuery,
            deferredEmailQuery,
            subscriptionFilter,
          ),
        )
        .sort((left, right) => {
          const comparison = compareProfiles(
            left.profile,
            right.profile,
            sortKey,
            sortDirection,
          );
          return comparison || left.index - right.index;
        })
        .map(({ profile }) => profile),
    [
      deferredEmailQuery,
      deferredNameQuery,
      profiles,
      sortDirection,
      sortKey,
      subscriptionFilter,
    ],
  );
  const hasActiveFilters =
    Boolean(nameQuery || emailQuery) ||
    subscriptionFilter !== ALL_SUBSCRIPTIONS ||
    sortKey !== "default";

  const clearFilters = () => {
    setNameQuery("");
    setEmailQuery("");
    setSubscriptionFilter(ALL_SUBSCRIPTIONS);
    setSortKey("default");
    setSortDirection("urgent");
    setOpenMenu(null);
  };

  const closeFlow = useCallback(() => {
    completing.current = false;
    setFlow(null);
    setImportError(null);
    setJsonResult(null);
  }, []);

  const beginJsonImportWithPaths = useCallback(async (paths: string[]) => {
    setImportError(null);
    if (!paths.length) {
      setImportError("未检测到可导入的 JSON 文件。");
      setFlow({ step: "picker" });
      return;
    }
    setJsonBusy(true);
    try {
      const preview = await api.previewJsonProfileImport(paths);
      setJsonResult(null);
      setSelectedJsonItems(
        new Set(
          preview.items
            .filter((item) => item.status === "valid")
            .map((item) => item.id),
        ),
      );
      setFlow({ step: "json", preview });
    } catch {
      setImportError(
        "无法解析或验证所选 JSON。请确认文件格式、Codex CLI 和网络连接后重试。",
      );
      setFlow({ step: "picker" });
    } finally {
      setJsonBusy(false);
    }
  }, []);

  const beginJsonImport = async () => {
    setImportError(null);
    try {
      const selected = await open({
        title: "选择要导入的账号 JSON 文件",
        multiple: true,
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      const paths = Array.isArray(selected) ? selected : selected ? [selected] : [];
      if (!paths.length) return;
      await beginJsonImportWithPaths(paths);
    } catch {
      setImportError(
        "无法解析或验证所选 JSON。请确认文件格式、Codex CLI 和网络连接后重试。",
      );
      setFlow({ step: "picker" });
    }
  };
  const discardJsonPreview = async (previewId: string) => {
    try {
      await api.discardJsonProfileImport(previewId);
    } finally {
      closeFlow();
    }
  };
  const retryJsonPreview = async (preview: JsonProfileImportPreview) => {
    setImportError(null);
    setJsonBusy(true);
    try {
      const nextPreview = await api.retryJsonProfileImport(preview.preview_id);
      const previouslyUnverified = new Set(
        preview.items
          .filter((item) => item.status === "unverified")
          .map((item) => item.id),
      );
      setSelectedJsonItems(
        (current) =>
          new Set(
            nextPreview.items
              .filter(
                (item) =>
                  item.status === "valid" &&
                  (current.has(item.id) || previouslyUnverified.has(item.id)),
              )
              .map((item) => item.id),
          ),
      );
      setFlow({ step: "json", preview: nextPreview });
    } catch {
      setImportError("重试预检未完成。预览已失效时请重新选择文件。");
    } finally {
      setJsonBusy(false);
    }
  };
  const refreshImportedProfiles = async (result: JsonProfileImportResult) => {
    let failed = false;
    const imported = result.items.filter(
      (item) =>
        (item.action === "created" || item.action === "updated") && item.profile_id,
    );
    for (const item of imported) {
      const profileId = item.profile_id;
      if (!profileId) continue;
      try {
        await api.syncProfileAccountInfo(profileId);
        if (item.auth_mode === "oauth") await api.refreshProfileModels(profileId);
      } catch {
        failed = true;
      }
    }
    return failed;
  };

  const commitJsonPreview = async (preview: JsonProfileImportPreview) => {
    if (!selectedJsonItems.size) return;
    setJsonBusy(true);
    try {
      const result: JsonProfileImportResult = await api.commitJsonProfileImport(
        preview.preview_id,
        [...selectedJsonItems],
      );
      const refreshFailed = await refreshImportedProfiles(result);
      await onJsonImportComplete();
      setJsonResult(result);
      if (refreshFailed) {
        setImportError(
          "导入已完成，但部分账号资料或可用模型刷新未完成，可稍后手动刷新。",
        );
      }
    } catch {
      setImportError("导入未完成。预览已失效时请重新选择文件。");
    } finally {
      setJsonBusy(false);
    }
  };
  const beginOAuth = async (profileId?: string) => {
    setImportError(null);
    try {
      const status = await onStartOAuth(profileId);
      setFlow({ step: "authorizing", status });
    } catch {
      setImportError("无法启动官方登录。请确认 Codex CLI 与系统安全存储可用后重试。");
    }
  };
  const handleStatus = useCallback(
    async (status: OAuthImportStatus) => {
      if (status.phase === "authorizing") {
        setFlow({ step: "authorizing", status });
        return;
      }
      if (status.phase === "authenticated") {
        if (status.profile_id) {
          if (completing.current) return;
          completing.current = true;
          try {
            await onCompleteOAuth(status.attempt_id);
            closeFlow();
          } catch {
            setImportError("授权已完成，但无法更新档案状态。请重试或重新授权。");
          }
          return;
        }
        setFlow({ step: "naming", status });
        return;
      }
      setImportError(status.message);
      setFlow({ step: "picker" });
    },
    [closeFlow, onCompleteOAuth],
  );

  useEffect(() => {
    if (flow?.step !== "authorizing") return;
    const timer = window.setInterval(() => {
      void onOAuthStatus(flow.status.attempt_id)
        .then(handleStatus)
        .catch(() => {
          setImportError("无法读取授权状态；请取消后重试。");
        });
    }, 1000);
    return () => window.clearInterval(timer);
  }, [flow, handleStatus, onOAuthStatus]);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let unlisten: (() => void) | undefined;
    let disposed = false;
    try {
      void getCurrentWebview()
        .onDragDropEvent((event) => {
          if (event.payload.type !== "drop") return;
          const jsonPaths = event.payload.paths.filter((path) =>
            path.toLowerCase().endsWith(".json"),
          );
          if (!jsonPaths.length) {
            setImportError("拖入的文件不是 JSON；请拖入 .json 账号文件。");
            setFlow({ step: "picker" });
            return;
          }
          void beginJsonImportWithPaths(jsonPaths);
        })
        .then((dispose) => {
          if (disposed) dispose();
          else unlisten = dispose;
        })
        .catch(() => {
          // Native drag/drop is optional in browser-only tests and preview builds.
        });
    } catch {
      // Tauri globals may exist without the webview API in unit tests.
    }
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [beginJsonImportWithPaths]);

  const refreshAccount = async (id: string) => {
    if (refreshingProfileId) return;
    setRefreshingProfileId(id);
    try {
      await onSyncAccount(id);
    } catch {
      // App-level error handling presents a safe failure message and the
      // persisted profile state retains the last successful snapshot.
    } finally {
      setRefreshingProfileId(null);
    }
  };

  return (
    <div className="page profiles-page">
      <header className="page-heading" data-animate="heading">
        <div>
          <p className="section-kicker">Profiles</p>
          <h1>档案与账号池</h1>
          <p className="page-subtitle">
            档案凭据仅保存到系统安全存储；这里始终显示掩码和状态。
          </p>
        </div>
        <button
          className="primary-button"
          type="button"
          onClick={() => setFlow({ step: "picker" })}
        >
          <Plus size={18} weight="bold" /> 添加档案
        </button>
      </header>
      <section className="toolbar profile-filter-toolbar" data-animate="toolbar">
        <div className="profile-filter-fields">
          <label className="profile-filter-field">
            <span>档案名称</span>
            <input
              value={nameQuery}
              onChange={(event) => setNameQuery(event.target.value)}
              placeholder="按档案名称筛选"
            />
          </label>
          <label className="profile-filter-field">
            <span>邮箱</span>
            <input
              value={emailQuery}
              onChange={(event) => setEmailQuery(event.target.value)}
              placeholder="按邮箱筛选"
              type="search"
            />
          </label>
          <div className="profile-filter-field">
            <span>订阅类型</span>
            <FilterMenu
              id="subscription-filter"
              label="订阅类型"
              onChange={setSubscriptionFilter}
              onOpenChange={(isOpen) => setOpenMenu(isOpen ? "subscription" : null)}
              open={openMenu === "subscription"}
              options={subscriptionOptions}
              value={subscriptionFilter}
            />
          </div>
          <div className="profile-filter-field profile-sort-field">
            <span>排序方式</span>
            <div className="profile-sort-controls">
              <FilterMenu
                id="profile-sort"
                label="排序方式"
                onChange={(nextSortKey) => {
                  setSortKey(nextSortKey);
                  if (nextSortKey !== "default") setSortDirection("urgent");
                }}
                onOpenChange={(isOpen) => setOpenMenu(isOpen ? "sort" : null)}
                open={openMenu === "sort"}
                options={PROFILE_SORT_OPTIONS}
                value={sortKey}
              />
              <button
                aria-label={
                  sortKey === "default"
                    ? "选择排序方式后可切换排序方向"
                    : `当前${sortDirection === "urgent" ? "紧急优先" : "高值优先"}，点击切换`
                }
                className="profile-sort-direction"
                disabled={sortKey === "default"}
                onClick={() =>
                  setSortDirection((direction) =>
                    direction === "urgent" ? "reverse" : "urgent",
                  )
                }
                title={
                  sortKey === "default"
                    ? "选择排序方式后可切换排序方向"
                    : sortDirection === "urgent"
                      ? "紧急优先"
                      : "高值优先"
                }
                type="button"
              >
                {sortDirection === "urgent" ? (
                  <ArrowDown size={16} />
                ) : (
                  <ArrowUp size={16} />
                )}
              </button>
            </div>
          </div>
        </div>
        <div className="profile-filter-summary">
          <span aria-live="polite" className="toolbar-count">
            显示 {visibleProfiles.length} / 共 {profiles.length} 个档案
          </span>
          {hasActiveFilters && (
            <button
              className="text-button profile-filter-clear"
              onClick={clearFilters}
              type="button"
            >
              清除筛选
            </button>
          )}
        </div>
      </section>
      <section className="privacy-banner" data-animate="notice">
        <CloudArrowUp size={23} weight="fill" />
        <div>
          <strong>当前档案会按所选模式启动 Codex 工作区</strong>
          <p>
            添加账号时会保存认证凭据。当前模式为“{workspaceModeLabel(workspaceMode)}
            ”；切换会更新默认 .codex/auth.json 与 Codex Auth 钥匙串，不会迁移 ChatGPT
            Chat/Work 的独立登录会话。
          </p>
        </div>
      </section>
      {flow?.step === "json" ? (
        <JsonImportSheet
          busy={busy || jsonBusy}
          error={importError}
          preview={flow.preview}
          result={jsonResult}
          selected={selectedJsonItems}
          onClose={() => void discardJsonPreview(flow.preview.preview_id)}
          onCommit={() => void commitJsonPreview(flow.preview)}
          onRetry={() => void retryJsonPreview(flow.preview)}
          onToggle={(id) =>
            setSelectedJsonItems((current) => {
              const next = new Set(current);
              if (next.has(id)) next.delete(id);
              else next.add(id);
              return next;
            })
          }
          onSelectAll={() =>
            setSelectedJsonItems(
              new Set(
                flow.preview.items
                  .filter((item) => item.status === "valid")
                  .map((item) => item.id),
              ),
            )
          }
        />
      ) : flow?.step === "api" ? (
        <ApiProfileSheet
          busy={busy}
          onClose={closeFlow}
          onSubmit={async (input) => {
            await onCreateApiProfile(input);
            closeFlow();
          }}
        />
      ) : flow ? (
        <OAuthImportSheet
          flow={flow}
          busy={busy}
          error={importError}
          onClose={closeFlow}
          onStart={() => void beginOAuth()}
          onOpenJson={() => void beginJsonImport()}
          onOpenApi={() => setFlow({ step: "api" })}
          onCancel={async (attemptId) => {
            try {
              await onCancelOAuth(attemptId);
              closeFlow();
            } catch {
              setImportError("无法取消登录；请关闭官方登录页后重试。");
            }
          }}
          onComplete={async (attemptId, alias) => {
            try {
              await onCompleteOAuth(attemptId, alias);
              closeFlow();
            } catch {
              setImportError("无法保存档案。凭据未被复制到应用中，请修改名称后重试。");
            }
          }}
        />
      ) : null}
      <section className="profile-grid" data-animate="cards">
        {visibleProfiles.map((profile) => (
          <ProfileCard
            key={profile.id}
            profile={profile}
            busy={busy}
            onSelect={onSelect}
            onReauthorize={() => void beginOAuth(profile.id)}
            onSyncAccount={() => refreshAccount(profile.id)}
            onTogglePool={() => onTogglePool(profile)}
            onActivateApiProfile={() => onActivateApiProfile(profile)}
            refreshing={refreshingProfileId === profile.id}
            onDelete={onDelete}
          />
        ))}
        {!visibleProfiles.length && (
          <article className="empty-state">
            <CloudArrowUp size={38} weight="duotone" />
            {profiles.length ? (
              <>
                <h2>没有匹配的档案</h2>
                <p>请调整筛选条件，或清除筛选后查看全部档案。</p>
                <button className="quiet-button" onClick={clearFilters} type="button">
                  清除筛选
                </button>
              </>
            ) : (
              <>
                <h2>从一个已获授权的连接开始</h2>
                <p>通过官方 OpenAI / ChatGPT OAuth 登录创建受管 Codex 档案。</p>
                <button
                  className="primary-button"
                  type="button"
                  onClick={() => setFlow({ step: "picker" })}
                >
                  <Plus size={17} /> 添加档案
                </button>
              </>
            )}
          </article>
        )}
      </section>
    </div>
  );
}

function FilterMenu<T extends string>({
  id,
  label,
  onChange,
  onOpenChange,
  open,
  options,
  value,
}: {
  id: string;
  label: string;
  onChange: (value: T) => void;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  options: FilterMenuOption<T>[];
  value: T;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const optionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const selectedIndex = Math.max(
    options.findIndex((option) => option.value === value),
    0,
  );

  useEffect(() => {
    if (!open) return;

    optionRefs.current[selectedIndex]?.focus();
  }, [open, selectedIndex]);

  useEffect(() => {
    if (!open) return;

    const closeOnOutsidePointerDown = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !containerRef.current?.contains(event.target)
      ) {
        onOpenChange(false);
      }
    };

    window.addEventListener("pointerdown", closeOnOutsidePointerDown);
    return () => window.removeEventListener("pointerdown", closeOnOutsidePointerDown);
  }, [onOpenChange, open]);

  const closeAndRestoreFocus = () => {
    onOpenChange(false);
    triggerRef.current?.focus();
  };
  const selectOption = (option: FilterMenuOption<T>) => {
    onChange(option.value);
    closeAndRestoreFocus();
  };
  const moveFocus = (index: number) => {
    optionRefs.current[(index + options.length) % options.length]?.focus();
  };
  const handleOptionKeyDown = (
    event: KeyboardEvent<HTMLButtonElement>,
    index: number,
  ) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveFocus(index + 1);
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveFocus(index - 1);
      return;
    }
    if (event.key === "Home") {
      event.preventDefault();
      moveFocus(0);
      return;
    }
    if (event.key === "End") {
      event.preventDefault();
      moveFocus(options.length - 1);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeAndRestoreFocus();
      return;
    }
    if (event.key === "Tab") {
      onOpenChange(false);
      return;
    }
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      selectOption(options[index]);
    }
  };
  const selectedOption = options[selectedIndex];

  return (
    <div className={`profile-menu ${open ? "is-open" : ""}`} ref={containerRef}>
      <button
        aria-controls={`${id}-listbox`}
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-label={`${label}：${selectedOption.label}`}
        className="profile-menu-trigger"
        onClick={() => onOpenChange(!open)}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            onOpenChange(true);
          }
        }}
        ref={triggerRef}
        type="button"
      >
        <span>{selectedOption.label}</span>
        <CaretDown aria-hidden="true" className="profile-menu-caret" size={16} />
      </button>
      {open && (
        <div
          aria-label={`${label}选项`}
          className="profile-menu-list"
          id={`${id}-listbox`}
          role="listbox"
        >
          {options.map((option, index) => {
            const selected = option.value === value;
            return (
              <button
                aria-selected={selected}
                className="profile-menu-option"
                key={option.value}
                onClick={(event) => {
                  if (event.detail === 0) selectOption(option);
                }}
                onKeyDown={(event) => handleOptionKeyDown(event, index)}
                onPointerDown={(event) => {
                  event.preventDefault();
                  selectOption(option);
                }}
                ref={(element) => {
                  optionRefs.current[index] = element;
                }}
                role="option"
                type="button"
              >
                <span>{option.label}</span>
                {selected && <Check aria-hidden="true" size={16} weight="bold" />}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

function matchesProfileFilters(
  profile: MaskedProfile,
  nameQuery: string,
  emailQuery: string,
  subscriptionFilter: string,
) {
  const normalizedName = nameQuery.trim().toLowerCase();
  const normalizedEmail = emailQuery.trim().toLowerCase();
  const profileEmail = profile.account?.email?.toLowerCase() ?? "";
  const planType = profile.account?.subscription.plan_type ?? null;

  return (
    (!normalizedName || profile.alias.toLowerCase().includes(normalizedName)) &&
    (!normalizedEmail || profileEmail.includes(normalizedEmail)) &&
    (subscriptionFilter === ALL_SUBSCRIPTIONS ||
      (subscriptionFilter === UNSYNCED_SUBSCRIPTION
        ? profile.kind === "codex_oauth" && !planType
        : planType === subscriptionFilter))
  );
}

function compareProfiles(
  left: MaskedProfile,
  right: MaskedProfile,
  sortKey: ProfileSortKey,
  sortDirection: ProfileSortDirection,
) {
  if (sortKey === "default") return 0;

  const leftValue = profileSortValue(left, sortKey);
  const rightValue = profileSortValue(right, sortKey);
  if (leftValue === null) return rightValue === null ? 0 : 1;
  if (rightValue === null) return -1;

  const comparison = leftValue - rightValue;
  return sortDirection === "urgent" ? comparison : -comparison;
}

function profileSortValue(
  profile: MaskedProfile,
  sortKey: Exclude<ProfileSortKey, "default">,
) {
  if (sortKey === "subscription") {
    return profile.account?.subscription.period_ends_at_ms ?? null;
  }

  const windows = quotaWindows(profile.account?.quota);
  const values = windows
    .map((window) =>
      sortKey === "quota" ? window.remaining_percent : window.resets_at_ms,
    )
    .filter(
      (value): value is number => typeof value === "number" && Number.isFinite(value),
    );

  return values.length ? Math.min(...values) : null;
}

function quotaWindows(quota: ProfileQuota | null | undefined) {
  const bucketWindows =
    quota?.buckets.flatMap((bucket) => [bucket.primary, bucket.secondary]) ?? [];
  const windows = bucketWindows.length
    ? bucketWindows
    : [quota?.primary, quota?.secondary];
  return windows.filter((window): window is ProfileQuotaWindow => Boolean(window));
}

function workspaceModeLabel(mode: DesktopWorkspaceMode) {
  return {
    fresh: "每次全新启动",
    per_profile: "账号独立工作区",
    shared: "共享原客户端状态",
  }[mode];
}

function OAuthImportSheet({
  flow,
  busy,
  error,
  onClose,
  onStart,
  onOpenJson,
  onOpenApi,
  onCancel,
  onComplete,
}: {
  flow: Exclude<
    ImportFlow,
    null | { step: "json"; preview: JsonProfileImportPreview } | { step: "api" }
  >;
  busy: boolean;
  error: string | null;
  onClose: () => void;
  onStart: () => void;
  onOpenJson: () => void;
  onOpenApi: () => void;
  onCancel: (attemptId: string) => Promise<void>;
  onComplete: (attemptId: string, alias: string) => Promise<void>;
}) {
  const [alias, setAlias] = useState("");
  const dismiss = () => {
    if (flow.step === "authorizing" || flow.step === "naming") {
      void onCancel(flow.status.attempt_id);
      return;
    }
    onClose();
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (flow.step === "naming") void onComplete(flow.status.attempt_id, alias);
  };
  return (
    <section className="form-sheet" aria-labelledby="oauth-import-title">
      <div className="form-sheet-heading">
        <div>
          <p className="section-kicker">Import profile</p>
          <h2 id="oauth-import-title">
            {flow.step === "naming" ? "为新档案命名" : "选择导入方式"}
          </h2>
        </div>
        {flow.step === "authorizing" ? (
          <button
            className="text-button"
            type="button"
            onClick={() => void onCancel(flow.status.attempt_id)}
          >
            取消登录
          </button>
        ) : (
          <button className="text-button" type="button" onClick={dismiss}>
            取消
          </button>
        )}
      </div>
      {error && <p className="form-note error-note">{error}</p>}
      {flow.step === "picker" && (
        <div className="import-method-grid">
          <article className="import-method-card import-method-card-primary">
            <span className="import-method-icon" aria-hidden="true">
              <Key size={20} weight="duotone" />
            </span>
            <div>
              <strong>使用 OpenAI / ChatGPT 登录</strong>
              <p>
                在默认浏览器完成官方 OAuth 授权；凭据会保存到 Relay
                本地加密凭据库，后续切换无需再次登录。
              </p>
            </div>
            <button
              className="primary-button"
              type="button"
              disabled={busy}
              onClick={onStart}
            >
              继续使用 OAuth
            </button>
          </article>
          <article className="import-method-card">
            <span className="import-method-icon" aria-hidden="true">
              <CloudArrowUp size={20} weight="duotone" />
            </span>
            <div>
              <strong>从 JSON 文件导入</strong>
              <p>
                支持 auth.json、session、Sub2API 导出、完整或部分 token、PAT 与 Agent
                Identity。
              </p>
            </div>
            <button
              className="quiet-button"
              type="button"
              disabled={busy}
              onClick={onOpenJson}
            >
              选择 JSON 文件
            </button>
          </article>
          <article className="import-method-card">
            <span className="import-method-icon" aria-hidden="true">
              <CloudArrowUp size={20} weight="duotone" />
            </span>
            <div>
              <strong>添加第三方模型提供商</strong>
              <p>
                配置 Responses 或 Chat Completions 上游，测试后生成 Codex 可见模型映射。
              </p>
            </div>
            <button
              className="quiet-button"
              type="button"
              disabled={busy}
              onClick={onOpenApi}
            >
              添加提供商
            </button>
          </article>
        </div>
      )}
      {flow.step === "authorizing" && (
        <div className="oauth-progress" role="status">
          <strong>等待默认浏览器授权完成</strong>
          <p>{flow.status.message}</p>
          <p>请在默认浏览器完成登录。凭据会在创建档案后保存，后续切换无需重新登录。</p>
        </div>
      )}
      {flow.step === "naming" && (
        <form onSubmit={submit}>
          <label>
            档案名称
            <input
              required
              autoFocus
              value={alias}
              onChange={(event) => setAlias(event.target.value)}
              placeholder="例如：个人 OpenAI"
            />
          </label>
          <p className="form-note">模型能力将先标记为未知，且不会自动加入账号池。</p>
          <div className="form-actions">
            <button className="quiet-button" type="button" onClick={dismiss}>
              取消
            </button>
            <button className="primary-button" disabled={busy} type="submit">
              创建档案
            </button>
          </div>
        </form>
      )}
    </section>
  );
}

interface ApiProviderPreset {
  id: string;
  label: string;
  provider: GatewayProvider;
  wireApi: GatewayWireApi;
  baseUrl: string;
}

const API_PROVIDER_PRESETS: ApiProviderPreset[] = [
  {
    id: "custom_responses",
    label: "Custom Responses",
    provider: "openai_compatible",
    wireApi: "responses",
    baseUrl: "",
  },
  {
    id: "custom_chat",
    label: "Custom Chat Completions",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "",
  },
  {
    id: "deepseek",
    label: "DeepSeek",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://api.deepseek.com/v1",
  },
  {
    id: "kimi",
    label: "Kimi / Moonshot",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://api.moonshot.cn/v1",
  },
  {
    id: "glm",
    label: "GLM / BigModel",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://open.bigmodel.cn/api/paas/v4",
  },
  {
    id: "minimax",
    label: "MiniMax",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://api.minimax.io/v1",
  },
  {
    id: "qwen_bailian",
    label: "Qwen / Bailian",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1",
  },
  {
    id: "siliconflow",
    label: "SiliconFlow",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://api.siliconflow.cn/v1",
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    provider: "openai_compatible",
    wireApi: "chat_completions",
    baseUrl: "https://openrouter.ai/api/v1",
  },
  {
    id: "anthropic",
    label: "Anthropic Messages",
    provider: "anthropic",
    wireApi: "chat_completions",
    baseUrl: "https://api.anthropic.com",
  },
  {
    id: "gemini",
    label: "Gemini generateContent",
    provider: "gemini",
    wireApi: "chat_completions",
    baseUrl: "https://generativelanguage.googleapis.com",
  },
  {
    id: "ollama",
    label: "Ollama Local",
    provider: "ollama",
    wireApi: "chat_completions",
    baseUrl: "http://127.0.0.1:11434",
  },
];

const PROVIDER_CAPABILITY_NOTES: Record<GatewayProvider, string> = {
  openai:
    "OpenAI direct：Responses / Chat Completions 按所选 wire API 透传，支持上游原生参数与模型名回写。",
  openai_compatible:
    "OpenAI 兼容 direct：Responses / Chat Completions 按所选 wire API 透传；第三方非等价参数由上游决定。",
  anthropic:
    "Anthropic adapter：映射 Messages、system/content blocks、图片 data URL、tools/tool_choice、tool_use/tool_result、SSE 与 usage；不支持 audio/logprobs/top_logprobs。",
  gemini:
    "Gemini adapter：映射 v1beta generateContent、systemInstruction、parts/inlineData、functionDeclarations、JSON schema、SSE 与 usageMetadata；不支持 audio/logprobs/top_logprobs。",
  ollama:
    "Ollama adapter：OpenAI Chat/Responses 统一落到 /api/chat，映射 tools、format/schema、options、think、newline JSON streaming 与 token 统计；/api/generate 保留 native 透传。",
};

function identityMappings(models: string[]): GatewayModelMapping[] {
  return models.map((model) => ({
    model,
    upstream_model: model,
    display_name: model,
    context_window: null,
  }));
}

function ApiProfileSheet({
  busy,
  onClose,
  onSubmit,
}: {
  busy: boolean;
  onClose: () => void;
  onSubmit: (input: Record<string, unknown>) => Promise<void>;
}) {
  const [alias, setAlias] = useState("");
  const [presetId, setPresetId] = useState(API_PROVIDER_PRESETS[0].id);
  const [provider, setProvider] = useState<GatewayProvider>("openai_compatible");
  const [wireApi, setWireApi] = useState<GatewayWireApi>("responses");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [mappings, setMappings] = useState<GatewayModelMapping[]>([]);
  const [report, setReport] = useState<ApiServiceTestReport | null>(null);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);

  const applyPreset = (id: string) => {
    const preset = API_PROVIDER_PRESETS.find((candidate) => candidate.id === id);
    if (!preset) return;
    setPresetId(id);
    setProvider(preset.provider);
    setWireApi(preset.wireApi);
    setBaseUrl(preset.baseUrl);
    setReport(null);
    setMappings([]);
    setLocalError(null);
  };

  const test = async () => {
    setTesting(true);
    setLocalError(null);
    try {
      const nextReport = await api.testApiServiceProfile({
        provider,
        base_url: baseUrl,
        api_key: apiKey,
      });
      setReport(nextReport);
      if (nextReport.status === "verified") {
        setMappings(identityMappings(nextReport.models));
      }
    } catch (error) {
      setReport(null);
      setLocalError(error instanceof Error ? error.message : "连接测试未完成。");
    } finally {
      setTesting(false);
    }
  };

  const updateMapping = (index: number, patch: Partial<GatewayModelMapping>) => {
    setMappings((current) =>
      current.map((mapping, candidateIndex) =>
        candidateIndex === index ? { ...mapping, ...patch } : mapping,
      ),
    );
  };

  const addMapping = () => {
    setMappings((current) => [
      ...current,
      { model: "", upstream_model: "", display_name: null, context_window: null },
    ]);
  };

  const removeMapping = (index: number) => {
    setMappings((current) =>
      current.filter((_, candidateIndex) => candidateIndex !== index),
    );
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setLocalError(null);
    const normalizedMappings = mappings
      .map((mapping) => ({
        model: mapping.model.trim(),
        upstream_model: mapping.upstream_model.trim(),
        display_name: mapping.display_name?.trim() || null,
        context_window: mapping.context_window,
      }))
      .filter((mapping) => mapping.model && mapping.upstream_model);
    if (!normalizedMappings.length) {
      setLocalError("请先测试连接并保留至少一个模型映射。");
      return;
    }
    setSaving(true);
    try {
      await onSubmit({
        alias,
        provider,
        wire_api: wireApi,
        base_url: baseUrl,
        api_key: apiKey,
        model_mappings: normalizedMappings,
        in_pool: false,
        priority: 0,
        weight: 1,
      });
      onClose();
    } catch (error) {
      setLocalError(
        error instanceof Error ? error.message : "第三方模型提供商保存未完成。",
      );
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="form-sheet" aria-labelledby="api-profile-title">
      <div className="form-sheet-heading">
        <div>
          <p className="section-kicker">Third-party provider</p>
          <h2 id="api-profile-title">添加第三方模型提供商</h2>
        </div>
        <button className="text-button" type="button" onClick={onClose}>
          取消
        </button>
      </div>
      <form onSubmit={submit}>
        <label>
          档案名称
          <input
            required
            value={alias}
            onChange={(event) => setAlias(event.target.value)}
            placeholder="例如：DeepSeek Coding"
          />
        </label>
        <label>
          供应商预设
          <Select
            ariaLabel="供应商预设"
            onValueChange={applyPreset}
            options={API_PROVIDER_PRESETS.map((preset) => ({
              value: preset.id,
              label: preset.label,
            }))}
            value={presetId}
          />
        </label>
        <label>
          上游协议
          <Select
            ariaLabel="上游协议"
            onValueChange={(value) => {
              const next = value as GatewayWireApi;
              setWireApi(next);
              setReport(null);
              setMappings([]);
            }}
            options={[
              { value: "responses", label: "OpenAI Responses · 可直连" },
              {
                value: "chat_completions",
                label: "Chat Completions · 需要 Relay 本地路由",
              },
            ]}
            value={wireApi}
          />
        </label>
        <label>
          Provider 类型
          <Select
            ariaLabel="Provider 类型"
            onValueChange={(value) => {
              setProvider(value as GatewayProvider);
              setReport(null);
              setMappings([]);
            }}
            options={[
              { value: "openai", label: "OpenAI" },
              { value: "openai_compatible", label: "OpenAI 兼容" },
              { value: "anthropic", label: "Anthropic" },
              { value: "gemini", label: "Gemini" },
              { value: "ollama", label: "Ollama" },
            ]}
            value={provider}
          />
        </label>
        <label>
          Base URL
          <input
            required
            value={baseUrl}
            onChange={(event) => {
              setBaseUrl(event.target.value);
              setReport(null);
              setMappings([]);
            }}
            placeholder="https://api.example.com/v1"
          />
        </label>
        <label>
          API Key
          <input
            required
            type="password"
            value={apiKey}
            onChange={(event) => {
              setApiKey(event.target.value);
              setReport(null);
            }}
          />
        </label>
        <p className="form-note">{PROVIDER_CAPABILITY_NOTES[provider]}</p>
        <p className="form-note">
          模型映射会生成 Codex model_catalog_json，并决定网关对客户端暴露的 Model
          ID；修改后需重启 Codex 刷新 /model 列表。
        </p>
        {localError && <p className="form-note error-note">{localError}</p>}
        {report && (
          <div
            className={`profile-test-report ${
              report.status === "verified" ? "is-success" : "is-error"
            }`}
          >
            <strong>
              {report.status === "verified" ? "连接已验证" : "连接未验证"}
            </strong>
            <p>{report.message}</p>
            <p>
              {report.endpoint} · {report.latency_ms}ms
              {report.http_status ? ` · HTTP ${report.http_status}` : ""}
            </p>
          </div>
        )}
        <div className="model-mapping-editor" aria-label="模型映射">
          <div className="model-mapping-heading">
            <div>
              <strong>模型映射</strong>
              <p>
                Model ID 是 Codex 可见名称；Upstream Model 是发送给第三方的真实模型。
              </p>
            </div>
            <button className="quiet-button" type="button" onClick={addMapping}>
              添加模型
            </button>
          </div>
          {mappings.length ? (
            <div className="model-mapping-list">
              {mappings.map((mapping, index) => (
                <div className="model-mapping-row" key={`${mapping.model}-${index}`}>
                  <label>
                    Model ID
                    <input
                      value={mapping.model}
                      onChange={(event) =>
                        updateMapping(index, { model: event.target.value })
                      }
                      placeholder="Codex 显示的模型名"
                    />
                  </label>
                  <label>
                    Upstream Model
                    <input
                      value={mapping.upstream_model}
                      onChange={(event) =>
                        updateMapping(index, { upstream_model: event.target.value })
                      }
                      placeholder="第三方真实模型名"
                    />
                  </label>
                  <label>
                    Display Name
                    <input
                      value={mapping.display_name ?? ""}
                      onChange={(event) =>
                        updateMapping(index, {
                          display_name: event.target.value || null,
                        })
                      }
                      placeholder="可选"
                    />
                  </label>
                  <label>
                    Context Window
                    <input
                      min={1}
                      type="number"
                      value={mapping.context_window ?? ""}
                      onChange={(event) =>
                        updateMapping(index, {
                          context_window: event.target.value
                            ? Number(event.target.value)
                            : null,
                        })
                      }
                      placeholder="例如：128000"
                    />
                  </label>
                  <button
                    aria-label={`删除模型映射 ${index + 1}`}
                    className="icon-button danger"
                    type="button"
                    onClick={() => removeMapping(index)}
                  >
                    <Trash size={17} />
                  </button>
                </div>
              ))}
            </div>
          ) : (
            <p className="form-note">
              测试连接后会按上游返回模型自动生成 identity mapping。
            </p>
          )}
        </div>
        <div className="form-actions">
          <button className="quiet-button" type="button" onClick={onClose}>
            取消
          </button>
          <button
            className="quiet-button"
            disabled={busy || testing || saving || !baseUrl || !apiKey}
            type="button"
            onClick={() => void test()}
          >
            {testing ? "正在发现…" : "测试连接并发现模型"}
          </button>
          <button
            className="primary-button"
            disabled={busy || testing || saving || !mappings.length}
            type="submit"
          >
            {saving ? "正在保存…" : "测试并保存"}
          </button>
        </div>
      </form>
    </section>
  );
}

function JsonImportSheet({
  preview,
  result,
  selected,
  busy,
  error,
  onClose,
  onCommit,
  onRetry,
  onToggle,
  onSelectAll,
}: {
  preview: JsonProfileImportPreview;
  result: JsonProfileImportResult | null;
  selected: Set<string>;
  busy: boolean;
  error: string | null;
  onClose: () => void;
  onCommit: () => void;
  onRetry: () => void;
  onToggle: (id: string) => void;
  onSelectAll: () => void;
}) {
  const validItems = preview.items.filter((item) => item.status === "valid");
  const hasUnverifiedItems = preview.items.some((item) => item.status === "unverified");
  return (
    <section className="form-sheet" aria-labelledby="json-import-title">
      <div className="form-sheet-heading">
        <div>
          <p className="section-kicker">JSON import</p>
          <h2 id="json-import-title">选择要导入的账号</h2>
        </div>
        <button className="text-button" disabled={busy} type="button" onClick={onClose}>
          取消
        </button>
      </div>
      <p className="form-note">
        已完成本地解析和联网验证。凭据不会显示在此处，只有勾选的有效账号会写入系统安全存储。
      </p>
      {error && <p className="form-note error-note">{error}</p>}
      {result && (
        <p
          className={result.failed ? "form-note error-note" : "form-note success-note"}
        >
          导入完成：创建 {result.created}，更新 {result.updated}，跳过 {result.skipped}
          ，失败 {result.failed}。
        </p>
      )}
      <div className="form-actions">
        <button
          className="quiet-button"
          disabled={busy}
          type="button"
          onClick={onSelectAll}
        >
          全选有效项 ({validItems.length})
        </button>
        {hasUnverifiedItems && (
          <button
            className="quiet-button"
            disabled={busy}
            type="button"
            onClick={onRetry}
          >
            重试未验证项
          </button>
        )}
      </div>
      <div className="json-import-list">
        {preview.items.map((item) => {
          const selectable = item.status === "valid";
          return (
            <label className="json-import-item" key={item.id}>
              <input
                checked={selected.has(item.id)}
                disabled={!selectable || busy}
                onChange={() => onToggle(item.id)}
                type="checkbox"
              />
              <span>
                <strong>{item.alias}</strong>
                <small>
                  {item.file_name} · {authModeLabel(item.auth_mode)} · {item.source}
                  {item.existing_profile_alias
                    ? ` · 更新 ${item.existing_profile_alias}`
                    : ""}
                </small>
                <small
                  className={item.status === "valid" ? "success-note" : "error-note"}
                >
                  {item.message}
                </small>
              </span>
            </label>
          );
        })}
      </div>
      <div className="form-actions">
        <button
          className="quiet-button"
          disabled={busy}
          type="button"
          onClick={onClose}
        >
          取消
        </button>
        {!result && (
          <button
            className="primary-button"
            disabled={busy || !selected.size}
            type="button"
            onClick={onCommit}
          >
            导入 {selected.size} 个账号
          </button>
        )}
      </div>
    </section>
  );
}

function authModeLabel(mode: "oauth" | "agent_identity" | "personal_access_token") {
  return {
    oauth: "OAuth",
    agent_identity: "Agent Identity",
    personal_access_token: "个人访问令牌",
  }[mode];
}

function isGatewayCapableProfile(profile: MaskedProfile) {
  return profile.kind === "api_key" || profile.kind === "codex_oauth";
}

function ProfileCard({
  profile,
  busy,
  onSelect,
  onReauthorize,
  onSyncAccount,
  onTogglePool,
  onActivateApiProfile,
  refreshing,
  onDelete,
}: {
  profile: MaskedProfile;
  busy: boolean;
  onSelect: (id: string) => Promise<void>;
  onReauthorize: () => void;
  onSyncAccount: () => Promise<void>;
  onTogglePool: () => Promise<void>;
  onActivateApiProfile: () => Promise<void>;
  refreshing: boolean;
  onDelete: (id: string, alias: string) => void;
}) {
  const supportsManagedCurrentProfile =
    profile.kind === "codex_oauth" && profile.enabled && profile.credential_configured;
  const authMode = profile.auth_mode ?? "oauth";
  const gatewayCapable = isGatewayCapableProfile(profile);
  return (
    <article className={`profile-card ${profile.is_current ? "is-current" : ""}`}>
      <div className="profile-card-top">
        <div className="profile-avatar large">
          {profile.alias.slice(0, 1).toUpperCase()}
        </div>
        <div className="profile-title">
          <h2>{profile.alias}</h2>
          {profile.kind === "codex_oauth" ? (
            <p
              className="profile-header-email"
              title={profile.account?.email ?? undefined}
            >
              {profile.account?.email || "账号邮箱待同步"}
            </p>
          ) : (
            <span
              className={`status-pill compact ${profile.enabled ? "success" : "neutral"}`}
            >
              <i />
              {profile.enabled ? "已启用" : "已停用"}
            </span>
          )}
        </div>
        {profile.is_current && (
          <span className="current-badge">
            <CheckCircle size={15} weight="fill" /> 当前
          </span>
        )}
      </div>
      {profile.kind === "codex_oauth" && (
        <ProfileUsageCard
          account={profile.account}
          authMode={authMode}
          busy={busy}
          refreshing={refreshing}
          onRefresh={onSyncAccount}
        />
      )}
      {gatewayCapable && (
        <dl className="profile-facts">
          <div>
            <dt>可用模型</dt>
            <dd>{profile.models.length} 个</dd>
          </div>
          <div>
            <dt>网关状态</dt>
            <dd>{profile.in_pool ? "已加入账号池" : "未加入"}</dd>
          </div>
          <div>
            <dt>优先级 / 权重</dt>
            <dd>
              {profile.priority} / {profile.weight}
            </dd>
          </div>
          <div>
            <dt>冷却</dt>
            <dd>{profile.cooldown_until_ms ? "冷却中" : "可用"}</dd>
          </div>
        </dl>
      )}
      {!supportsManagedCurrentProfile && (
        <p className="profile-runtime-note">
          {profile.kind === "codex_oauth" && !profile.credential_configured
            ? "此档案的旧凭据无法迁移；请重新授权后再切换。"
            : "当前仅支持凭据已保存的 Codex 档案作为受管当前档案。"}
        </p>
      )}
      <div className="profile-actions">
        {gatewayCapable && (
          <button
            className={
              profile.in_pool
                ? "quiet-button compact-action"
                : "primary-button compact-action"
            }
            aria-label={`${profile.in_pool ? "移出网关账号池" : "加入网关账号池"}：${profile.alias}`}
            title={
              profile.in_pool ? "将此账号移出网关账号池" : "将此账号加入网关账号池"
            }
            disabled={busy}
            type="button"
            onClick={() => void onTogglePool()}
          >
            {profile.in_pool ? "移出网关" : "加入网关"}
          </button>
        )}
        {profile.kind === "api_key" && (
          <button
            className="icon-button"
            aria-label={`激活 API 服务：${profile.alias}`}
            disabled={busy || profile.health !== "healthy" || !profile.models.length}
            title="测试通过后，将此 API 服务激活到 Codex"
            onClick={() => void onActivateApiProfile()}
          >
            <UserSwitch size={19} />
          </button>
        )}
        <button
          className="icon-button"
          aria-label={`设为当前档案：${profile.alias}`}
          title={
            !supportsManagedCurrentProfile
              ? "当前仅支持凭据已保存的 Codex 档案用于受管会话"
              : profile.is_current
                ? "重新应用当前档案并启动独立 ChatGPT/Codex 工作区"
                : undefined
          }
          disabled={busy || !supportsManagedCurrentProfile}
          onClick={() => void onSelect(profile.id)}
        >
          <UserSwitch size={19} />
        </button>
        {profile.kind === "codex_oauth" && (
          <button
            className="icon-button"
            type="button"
            aria-label={`${profile.credential_configured ? "更新凭据" : "重新授权"}：${profile.alias}`}
            title={profile.credential_configured ? "更新凭据" : "重新授权"}
            disabled={busy}
            onClick={onReauthorize}
          >
            <Key size={18} />
          </button>
        )}
        <button
          className="icon-button danger"
          aria-label={`删除档案：${profile.alias}`}
          disabled={busy}
          onClick={() => onDelete(profile.id, profile.alias)}
        >
          <Trash size={19} />
        </button>
      </div>
    </article>
  );
}

function ProfileUsageCard({
  account,
  authMode,
  busy,
  refreshing,
  onRefresh,
}: {
  account: MaskedProfile["account"];
  authMode: "oauth" | "agent_identity" | "personal_access_token";
  busy: boolean;
  refreshing: boolean;
  onRefresh: () => Promise<void>;
}) {
  const quota = account?.quota;
  const subscription = account?.subscription;
  const keychainAuthorizationRequired =
    quota?.last_error === "后台同步未读取钥匙串；点击“同步资料”后可在系统弹窗中授权。";
  const buckets =
    quota?.buckets.filter((bucket) => bucket.primary || bucket.secondary) ?? [];
  const primaryBucket = buckets[0] ?? {
    primary: quota?.primary ?? null,
    secondary: quota?.secondary ?? null,
  };
  const windows = [
    ["短期", primaryBucket.primary],
    ["长期", primaryBucket.secondary],
  ] as const;
  const visibleWindows = windows.filter(
    (window): window is ["短期" | "长期", ProfileQuotaWindow] => Boolean(window[1]),
  );
  const extraWindowCount = buckets
    .slice(1)
    .reduce(
      (count, bucket) =>
        count + Number(Boolean(bucket.primary)) + Number(Boolean(bucket.secondary)),
      0,
    );
  const quotaState = quotaStateLabel(quota, keychainAuthorizationRequired);
  const subscriptionState = subscriptionStateLabel(subscription);
  const periodLabel = subscription?.will_renew ? "距下次续费" : "距到期";
  return (
    <section className="profile-usage-card" aria-label="套餐与额度">
      <div className="profile-usage-header">
        <div>
          <div className="profile-usage-labels">
            <span>套餐与额度</span>
            <span className="profile-auth-mode">{authModeLabel(authMode)}</span>
          </div>
          <strong>{subscriptionPlanLabel(subscription?.plan_type)}</strong>
          <p>
            {subscription?.period_ends_at_ms
              ? `${periodLabel} ${formatRemainingTime(subscription.period_ends_at_ms)}`
              : subscriptionState || "套餐资料尚未同步"}
          </p>
        </div>
        <button
          className="profile-refresh-button"
          type="button"
          disabled={busy || refreshing}
          onClick={() => void onRefresh()}
        >
          <ArrowsClockwise
            className={refreshing ? "is-spinning" : undefined}
            size={16}
          />
          {refreshing ? "正在刷新" : "刷新资料"}
        </button>
      </div>
      {visibleWindows.length > 0 ? (
        <>
          <div
            className={`quota-meter-grid ${visibleWindows.length === 1 ? "is-single" : ""}`}
          >
            {visibleWindows.map(([label, window]) => (
              <QuotaMeter key={label} label={label} window={window} />
            ))}
          </div>
          {extraWindowCount > 0 && (
            <p className="quota-extra">另有 {extraWindowCount} 个额度窗口</p>
          )}
        </>
      ) : (
        <div className="profile-usage-empty">
          <strong>{quotaState || "上游未返回额度"}</strong>
          <p>
            {keychainAuthorizationRequired
              ? "需要系统授权后刷新"
              : quota?.message || "该认证方式暂未返回可展示的 Codex 额度。"}
          </p>
        </div>
      )}
      <footer>
        <span>最近更新 {formatProfileUpdatedAt(account?.updated_at_ms)}</span>
        {quotaState && <span className="profile-data-state">{quotaState}</span>}
      </footer>
    </section>
  );
}

function formatProfileUpdatedAt(updatedAtMs?: number | null) {
  if (!updatedAtMs) return "未记录";
  return new Intl.DateTimeFormat("zh-CN", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(updatedAtMs);
}

function QuotaMeter({
  label,
  window,
}: {
  label: "短期" | "长期";
  window: ProfileQuotaWindow;
}) {
  const remaining = Math.round(Math.min(Math.max(window.remaining_percent, 0), 100));
  const tone = remaining <= 10 ? "critical" : remaining <= 30 ? "warning" : "healthy";
  return (
    <div className="quota-meter">
      <div className="quota-meter-heading">
        <span>{label}</span>
        <strong>{remaining}%</strong>
      </div>
      <div
        aria-label={`${label}额度剩余 ${remaining}%`}
        aria-valuemax={100}
        aria-valuemin={0}
        aria-valuenow={remaining}
        className={`quota-progress ${tone}`}
        role="progressbar"
      >
        <span style={{ width: `${remaining}%` }} />
      </div>
      <span className="quota-reset">{formatQuotaReset(window.resets_at_ms)}</span>
    </div>
  );
}

function subscriptionPlanLabel(planType?: string | null) {
  return (
    {
      free: "ChatGPT Free",
      go: "ChatGPT Go",
      plus: "ChatGPT Plus",
      pro: "ChatGPT Pro",
      prolite: "ChatGPT Pro Lite",
      team: "ChatGPT Team",
      self_serve_business_usage_based: "ChatGPT Business",
      business: "ChatGPT Business",
      enterprise_cbp_usage_based: "ChatGPT Enterprise",
      enterprise: "ChatGPT Enterprise",
      edu: "ChatGPT Edu",
      k12: "ChatGPT K-12 / Edu",
      unknown: "套餐尚未同步",
    }[planType ?? ""] ?? (planType ? `未知套餐（${planType}）` : "套餐尚未同步")
  );
}

function formatRemainingTime(periodEndsAtMs: number) {
  const remainingMinutes = Math.ceil((periodEndsAtMs - Date.now()) / 60_000);
  if (remainingMinutes <= 0) return "已到期";
  const days = Math.floor(remainingMinutes / (24 * 60));
  const hours = Math.floor((remainingMinutes % (24 * 60)) / 60);
  return days > 0 ? `剩余 ${days} 天 ${hours} 小时` : `剩余 ${Math.max(hours, 1)} 小时`;
}

function formatQuotaReset(resetsAtMs: number | null) {
  if (!resetsAtMs) return "重置时间未提供";
  const remainingMinutes = Math.ceil((resetsAtMs - Date.now()) / 60_000);
  if (remainingMinutes <= 0) return "正在重置";
  const days = Math.floor(remainingMinutes / (24 * 60));
  const hours = Math.floor((remainingMinutes % (24 * 60)) / 60);
  return days > 0 ? `约 ${days} 天后重置` : `约 ${Math.max(hours, 1)} 小时后重置`;
}

function quotaStateLabel(
  quota: ProfileQuota | null | undefined,
  keychainAuthorizationRequired: boolean,
) {
  if (keychainAuthorizationRequired) return "需要授权";
  if (!quota || quota.status === "unavailable") return "尚未同步";
  if (quota.rate_limit_reached_type) return "额度受限";
  if (quota.status === "stale") return "缓存已过期";
  return null;
}

function subscriptionStateLabel(subscription: ProfileSubscription | null | undefined) {
  if (!subscription || subscription.status === "unavailable") return "尚未同步";
  if (subscription.status === "stale") return "缓存已过期";
  if (subscription.will_renew === true) return "自动续费";
  if (subscription.will_renew === false) return "到期结束";
  return null;
}
