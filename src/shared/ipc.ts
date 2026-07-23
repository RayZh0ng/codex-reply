import { invoke } from "@tauri-apps/api/core";

export type ProfileKind = "api_key" | "codex_oauth";
export type ChannelKind = "feishu" | "wecom" | "custom";
export type DesktopWorkspaceMode = "fresh" | "per_profile" | "shared";

export interface MaskedProfile {
  id: string;
  alias: string;
  kind: ProfileKind;
  base_url: string | null;
  enabled: boolean;
  in_pool: boolean;
  priority: number;
  weight: number;
  models: string[];
  health: string;
  cooldown_until_ms: number | null;
  credential_configured: boolean;
  is_current: boolean;
  account?: ProfileAccountSummary | null;
}

export interface ProfileAccountSummary {
  display_name: string | null;
  email: string | null;
  account_id: string | null;
  updated_at_ms: number;
  quota: ProfileQuota;
  subscription: ProfileSubscription;
}

export interface ProfileQuota {
  status: "available" | "stale" | "unavailable";
  message: string;
  source: "app_server" | "usage_api" | null;
  synced_at_ms: number | null;
  last_attempt_at_ms: number;
  last_error: string | null;
  primary: ProfileQuotaWindow | null;
  secondary: ProfileQuotaWindow | null;
  buckets: ProfileQuotaBucket[];
  rate_limit_reached_type: string | null;
}

export interface ProfileQuotaWindow {
  used_percent: number;
  remaining_percent: number;
  window_duration_mins: number;
  resets_at_ms: number | null;
}

export interface ProfileQuotaBucket {
  id: string | null;
  name: string | null;
  plan_type: string | null;
  primary: ProfileQuotaWindow | null;
  secondary: ProfileQuotaWindow | null;
}

export interface ProfileSubscription {
  status: "available" | "stale" | "unavailable";
  plan_type: string | null;
  period_ends_at_ms: number | null;
  will_renew: boolean | null;
  source: "app_server" | "account_check" | "app_server+account_check" | null;
  synced_at_ms: number | null;
  last_attempt_at_ms: number;
  last_error: string | null;
}

export interface ProfileQuotaRefreshReport {
  profiles: MaskedProfile[];
  failed_profile_ids: string[];
}

export interface GatewayStatus {
  running: boolean;
  bind_mode: "loopback" | "lan";
  bind_address: string;
  port: number;
  cidrs: string[];
  available_profiles: number;
  cooling_profiles: number;
  client_key_count: number;
  certificate_ready: boolean;
}

export interface MetricsSnapshot {
  total_requests: number;
  successful_requests: number;
  failed_requests: number;
  average_latency_ms: number | null;
  estimated_tokens: number;
}

export interface MaskedChannel {
  id: string;
  name: string;
  kind: ChannelKind;
  enabled: boolean;
  endpoint_mask: string;
  last_status: string;
}

export interface MaskedClientKey {
  id: string;
  name: string;
  masked_value: string;
  created_at_ms: number;
  last_used_at_ms: number | null;
  revoked: boolean;
}

export interface DashboardSnapshot {
  gateway: GatewayStatus;
  profiles: MaskedProfile[];
  metrics: MetricsSnapshot;
  notifications: MaskedChannel[];
}

export interface CreatedClientKey {
  key: MaskedClientKey;
  plaintext_once: string;
}

export type OAuthImportPhase = "authorizing" | "authenticated" | "failed" | "cancelled";

export interface OAuthImportStatus {
  attempt_id: string;
  profile_id: string | null;
  phase: OAuthImportPhase;
  message: string;
}

export interface CurrentProfileActivation {
  profile_id: string;
  attempt_id: string | null;
  status:
    | "switching"
    | "activated"
    | "auth_file_write_failed"
    | "codex_keychain_write_failed"
    | "desktop_restart_failed"
    | "failed"
    | "cancelled";
  message: string;
}

export interface DesktopWorkspaceSettings {
  mode: DesktopWorkspaceMode;
}

export interface DesktopWorkspaceHistoryItem {
  id: string;
  profile_id: string;
  profile_alias: string;
  created_at_ms: number;
  last_launched_at_ms: number;
}

export type ManagedTaskPhase =
  "idle" | "running" | "completed" | "failed" | "cancelled";

export interface ManagedTaskStatus {
  phase: ManagedTaskPhase;
  profile_id: string | null;
  message: string;
}

export interface StartManagedTaskInput {
  instruction: string;
  working_directory: string;
}

export class RelayError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

function hasNativeRuntime() {
  return "__TAURI_INTERNALS__" in window;
}

export async function relayInvoke<T>(command: string, args?: Record<string, unknown>) {
  if (!hasNativeRuntime()) {
    throw new RelayError(
      "runtime_unavailable",
      "本机运行时不可用。请通过 Codex Relay 桌面应用启动此页面。",
    );
  }

  try {
    return await invoke<T>(command, args);
  } catch (error) {
    if (typeof error === "object" && error && "code" in error) {
      const details = error as { code?: unknown; message?: unknown };
      const code = typeof details.code === "string" ? details.code : "internal";
      const message =
        typeof details.message === "string" && details.message.trim()
          ? details.message
          : "本机操作未完成。";
      throw new RelayError(code, `${message}（错误码：${code}）${recoveryFor(code)}`);
    }
    throw new RelayError(
      "internal",
      "本机操作未完成。（错误码：internal）请重启 Codex Relay；若仍出现，请保留该错误码后重试。",
    );
  }
}

function recoveryFor(code: string) {
  return (
    {
      internal: "请重启 Codex Relay；若仍出现，请保留该错误码后重试。",
      runtime_unavailable: "请确认 Codex CLI 可用后重试。",
      secret_store_unavailable: "请解锁系统安全存储后重试。",
      keychain_interaction_required:
        "请在 macOS 系统弹窗中输入登录钥匙串密码，并选择“始终允许”。",
      profile_credential_migration_required:
        "该档案由旧版应用保存；请使用“更新凭据”重新完成一次 OAuth。",
      profile_runtime_unavailable: "请重新授权该 OAuth 档案后重试。",
      auth_file_write_failed: "请确认默认 .codex 目录可写后重试。",
      codex_keychain_unavailable:
        "请解锁 macOS 钥匙串并允许 Codex Relay 写入“Codex Auth”后重试。",
    }[code] ?? "请检查本机配置后重试。"
  );
}

export const api = {
  dashboard: () => relayInvoke<DashboardSnapshot>("dashboard_snapshot"),
  createProfile: (input: Record<string, unknown>) =>
    relayInvoke<MaskedProfile>("create_profile", { input }),
  updateProfile: (input: Record<string, unknown>) =>
    relayInvoke<MaskedProfile>("update_profile", { input }),
  syncProfileAccountInfo: (id: string) =>
    relayInvoke<MaskedProfile>("sync_profile_account_info", { id }),
  refreshProfileQuotas: () =>
    relayInvoke<ProfileQuotaRefreshReport>("refresh_profile_quotas"),
  deleteProfile: (id: string) =>
    relayInvoke<void>("delete_profile", { id, confirmed: true }),
  selectProfile: (id: string, confirmedDesktopRestart = false) =>
    relayInvoke<CurrentProfileActivation>("select_current_profile", {
      input: {
        id,
        confirmed_desktop_restart: confirmedDesktopRestart,
      },
    }),
  desktopWorkspaceSettings: () =>
    relayInvoke<DesktopWorkspaceSettings>("desktop_workspace_settings"),
  updateDesktopWorkspaceSettings: (mode: DesktopWorkspaceMode) =>
    relayInvoke<DesktopWorkspaceSettings>("update_desktop_workspace_settings", {
      input: { mode },
    }),
  listDesktopWorkspaces: () =>
    relayInvoke<DesktopWorkspaceHistoryItem[]>("list_desktop_workspaces"),
  restoreDesktopWorkspace: (id: string) =>
    relayInvoke<CurrentProfileActivation>("restore_desktop_workspace", {
      input: { id },
    }),
  deleteDesktopWorkspace: (id: string) =>
    relayInvoke<void>("delete_desktop_workspace", { input: { id, confirmed: true } }),
  currentProfileActivationStatus: (attemptId: string) =>
    relayInvoke<CurrentProfileActivation>("current_profile_activation_status", {
      input: { attempt_id: attemptId },
    }),
  managedTaskStatus: () => relayInvoke<ManagedTaskStatus>("managed_task_status"),
  startManagedTask: (input: StartManagedTaskInput) =>
    relayInvoke<ManagedTaskStatus>("start_managed_task", { input }),
  cancelManagedTask: () =>
    relayInvoke<ManagedTaskStatus>("cancel_managed_task", {
      input: { confirmed: true },
    }),
  updateGateway: (input: Record<string, unknown>) =>
    relayInvoke<GatewayStatus>("update_gateway", { input }),
  startGateway: () => relayInvoke<GatewayStatus>("start_gateway"),
  stopGateway: () => relayInvoke<GatewayStatus>("stop_gateway"),
  listClientKeys: () => relayInvoke<MaskedClientKey[]>("list_client_keys"),
  createClientKey: (name: string) =>
    relayInvoke<CreatedClientKey>("create_client_key", {
      input: { name, confirmed: true },
    }),
  revokeClientKey: (id: string) =>
    relayInvoke<void>("revoke_client_key", { id, confirmed: true }),
  upsertChannel: (input: Record<string, unknown>) =>
    relayInvoke<MaskedChannel>("upsert_channel", { input }),
  testChannel: (id: string) =>
    relayInvoke<MaskedChannel>("test_channel", { input: { id, confirmed: true } }),
  deleteChannel: (id: string) =>
    relayInvoke<void>("delete_channel", { id, confirmed: true }),
  startOAuthImport: (profileId?: string) =>
    relayInvoke<OAuthImportStatus>("start_oauth_import", {
      input: { profile_id: profileId ?? null },
    }),
  oauthImportStatus: (attemptId: string) =>
    relayInvoke<OAuthImportStatus>("oauth_import_status", { attemptId }),
  cancelOAuthImport: (attemptId: string) =>
    relayInvoke<void>("cancel_oauth_import", { attemptId }),
  completeOAuthImport: (attemptId: string, alias?: string) =>
    relayInvoke<MaskedProfile>("complete_oauth_import", {
      input: { attempt_id: attemptId, alias: alias ?? null },
    }),
};
