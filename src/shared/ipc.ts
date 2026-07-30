import { invoke } from "@tauri-apps/api/core";

export type ProfileKind = "api_key" | "codex_oauth";
export type GatewayProvider =
  "openai" | "openai_compatible" | "anthropic" | "gemini" | "ollama";
export type GatewayWireApi = "responses" | "chat_completions";
export type CodexAuthMode = "oauth" | "agent_identity" | "personal_access_token";
export type DesktopWorkspaceMode = "fresh" | "per_profile" | "shared";
export type AppUpdateChannel = "stable" | "beta";

export interface GatewayModelMapping {
  model: string;
  upstream_model: string;
  display_name: string | null;
  context_window: number | null;
}

export interface MaskedProfile {
  id: string;
  alias: string;
  kind: ProfileKind;
  base_url: string | null;
  provider?: GatewayProvider;
  wire_api?: GatewayWireApi;
  enabled: boolean;
  in_pool: boolean;
  priority: number;
  weight: number;
  models: string[];
  model_mappings?: GatewayModelMapping[];
  health: string;
  cooldown_until_ms: number | null;
  credential_configured: boolean;
  auth_mode?: CodexAuthMode;
  is_current: boolean;
  account?: ProfileAccountSummary | null;
}

export interface JsonProfileImportPreviewItem {
  id: string;
  file_name: string;
  alias: string;
  auth_mode: CodexAuthMode;
  source: string;
  status: "valid" | "invalid" | "unverified";
  message: string;
  email: string | null;
  account_id: string | null;
  existing_profile_alias: string | null;
}

export interface JsonProfileImportPreview {
  preview_id: string;
  expires_at_ms: number;
  items: JsonProfileImportPreviewItem[];
}

export interface JsonProfileImportResultItem {
  id: string;
  alias: string;
  action: "created" | "updated" | "skipped" | "failed";
  message: string;
  profile_id: string | null;
  auth_mode: CodexAuthMode | null;
}

export interface JsonProfileImportResult {
  created: number;
  updated: number;
  skipped: number;
  failed: number;
  items: JsonProfileImportResultItem[];
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
  source:
    | "app_server"
    | "account_check"
    | "app_server+account_check"
    | "import_verification"
    | null;
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
  bind_mode: "lan" | "loopback";
  bind_address: string;
  available_addresses: GatewayNetworkAddress[];
  port: number;
  cidrs: string[];
  available_profiles: number;
  cooling_profiles: number;
  client_key_count: number;
  certificate_ready: boolean;
  service_url: string;
  upstream_proxy_mode: "system" | "manual" | "disabled";
  upstream_proxy_display: string | null;
  upstream_last_error: string | null;
}

export interface GatewayNetworkAddress {
  name: string;
  address: string;
  is_default: boolean;
}

export interface GatewayOAuthProfileOption {
  id: string;
  alias: string;
  available: boolean;
  reason: string | null;
}

export interface GatewayCodexConfigStatus {
  enabled: boolean;
  config_path: string;
  service_url: string | null;
  message: string;
  auth_status: "ok" | "missing" | "legacy" | "invalid";
  needs_repair: boolean;
  oauth_profile_id: string | null;
  oauth_profile_alias: string | null;
  oauth_profile_available: boolean;
  oauth_profile_options: GatewayOAuthProfileOption[];
}

export interface ApiServiceTestReport {
  status: "verified" | "failed";
  category: string;
  endpoint: string;
  message: string;
  http_status: number | null;
  latency_ms: number;
  model_count: number;
  models: string[];
}

export interface CodexEnvironmentReport {
  platform: string;
  codex_home: string | null;
  can_install: boolean;
  message: string;
  last_checked_at_ms: number;
  summary: CodexEnvironmentSummary;
  checks: CodexEnvironmentCheck[];
  install_steps: CodexEnvironmentInstallStep[];
  manual_commands: string[];
}

export interface CodexEnvironmentSummary {
  status: "healthy" | "warning" | "action_required" | string;
  ok_count: number;
  warning_count: number;
  missing_count: number;
  failed_count: number;
  fixable_count: number;
  health_percent: number;
}

export interface CodexEnvironmentCheck {
  id: string;
  label: string;
  status: "ok" | "missing" | "warning" | "failed" | string;
  detail: string;
  command: string | null;
  description: string | null;
  next_action: string | null;
  automatic: boolean;
}

export interface CodexEnvironmentInstallStep {
  id: string;
  label: string;
  available: boolean;
  command: string | null;
  requires_privilege: boolean;
  next_action: string | null;
}

export interface CodexEnvironmentInstallReport {
  status: "completed" | "failed" | string;
  message: string;
  logs: CodexEnvironmentInstallLog[];
  environment: CodexEnvironmentReport;
}

export interface CodexEnvironmentInstallLog {
  step_id: string;
  label: string;
  status:
    "completed" | "failed" | "skipped" | "needs_privilege" | "unsupported" | string;
  detail: string;
  command: string | null;
  next_action: string | null;
}

export interface MetricsSnapshot {
  total_requests: number;
  successful_requests: number;
  failed_requests: number;
  average_latency_ms: number | null;
  estimated_tokens: number;
}

export interface MaskedClientKey {
  id: string;
  name: string;
  masked_value: string;
  created_at_ms: number;
  last_used_at_ms: number | null;
  revoked: boolean;
  managed_by: "user" | "codex_gateway";
  can_revoke: boolean;
}

export type CollaborationProvider = "feishu" | "qq" | "wecom" | "discord" | "telegram";
export type CollaborationExecutionTarget = "profile" | "gateway";
export type CollaborationBotStatus =
  | "disabled"
  | "configured"
  | "connecting"
  | "connected"
  | "callback_required"
  | "failed"
  | string;

export interface MaskedCollaborationBot {
  id: string;
  provider: CollaborationProvider;
  name: string;
  enabled: boolean;
  connection_status: CollaborationBotStatus;
  credential_mask: string;
  config_summary: string;
  callback_public_url: string | null;
  last_error: string | null;
  system_prompt: string | null;
  updated_at_ms: number;
}

export interface CollaborationProjectBinding {
  id: string;
  provider: CollaborationProvider;
  bot_id: string;
  bot_name: string;
  project_name: string;
  project_slug: string;
  working_directory: string;
  profile_id: string | null;
  profile_alias: string | null;
  chat_id: string | null;
  bind_code: string;
  enabled: boolean;
  concurrency_limit: number;
  execution_target: CollaborationExecutionTarget;
  model_id: string | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface CollaborationCallbackStatus {
  local_url: string;
  public_urls: string[];
  running: boolean;
}

export interface CollaborationContextSummary {
  id: string;
  scope_key: string;
  binding_id: string;
  provider: CollaborationProvider;
  bot_id: string;
  bot_name: string;
  project_name: string;
  project_slug: string;
  working_directory: string;
  execution_target: CollaborationExecutionTarget;
  profile_id: string | null;
  profile_alias: string | null;
  model_id: string | null;
  memory_enabled: boolean;
  permissions_policy: string;
  active_codex_session_id: string | null;
  active_relay_session_id: string | null;
  goal_status: string;
  goal_text: string | null;
  conversation_mode: string;
  last_turn_at_ms: number | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface MaskedFeishuBot {
  id: string;
  name: string;
  app_id: string;
  app_id_mask: string;
  enabled: boolean;
  connection_status: string;
  last_error: string | null;
  updated_at_ms: number;
}

export interface FeishuProjectBinding {
  id: string;
  bot_id: string;
  bot_name: string;
  project_name: string;
  project_slug: string;
  working_directory: string;
  profile_id: string;
  profile_alias: string;
  chat_id: string | null;
  bind_code: string;
  enabled: boolean;
  concurrency_limit: number;
  created_at_ms: number;
  updated_at_ms: number;
}

export type CodexSessionStatus =
  "running" | "completed" | "failed" | "cancelled" | string;

export interface CodexSessionSummary {
  id: string;
  binding_id: string;
  context_id: string | null;
  provider: CollaborationProvider;
  provider_bot_id: string | null;
  provider_chat_id: string | null;
  provider_message_id: string | null;
  project_name: string;
  project_slug: string;
  profile_id: string | null;
  profile_alias: string | null;
  relay_status: CodexSessionStatus;
  codex_session_id: string | null;
  feishu_message_id: string | null;
  feishu_chat_id: string | null;
  execution_target: CollaborationExecutionTarget;
  model_id: string | null;
  started_by: string | null;
  started_at_ms: number;
  updated_at_ms: number;
  finished_at_ms: number | null;
  summary: string | null;
  last_error: string | null;
  turn_kind: string;
  conversation_mode: string;
  goal_status: string | null;
}

export interface DashboardSnapshot {
  gateway: GatewayStatus;
  profiles: MaskedProfile[];
  metrics: MetricsSnapshot;
  workspace_mode: DesktopWorkspaceMode;
  collaboration: CollaborationSummary;
}

export interface CollaborationSummary {
  enabled_bots: number;
  bound_chats: number;
  active_sessions: number;
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

export interface AppUpdateSettings {
  channel: AppUpdateChannel;
  auto_check: boolean;
}

export interface AppUpdateInfo {
  version: string;
  current_version: string;
  body: string | null;
  date: string | null;
  channel: AppUpdateChannel;
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
    if (typeof error === "string" && error.trim()) {
      const code = "internal";
      throw new RelayError(
        code,
        `${error.trim()}（错误码：${code}）${recoveryFor(code)}`,
      );
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
      local_state_unavailable:
        "请在协作页刷新机器人连接；若仍出现，请重启 Codex Relay 并保留该错误码。",
      runtime_unavailable:
        "请在设置页运行 Codex 环境检查，确认 CLI、PATH 与本机目录可用后重试。",
      oauth_callback_port_unavailable:
        "请关闭占用 127.0.0.1:1455 的进程，或重启 Codex Relay 后重试。",
      oauth_browser_launch_failed:
        "请检查系统默认浏览器设置；Linux 可安装 xdg-utils/gio 后重试。",
      browser_launch_failed:
        "请设置系统默认浏览器；Linux 可安装 xdg-utils/gio 后重试。",
      environment_privilege_required:
        "该步骤需要系统授权；请按环境检查日志中的命令在终端执行。",
      environment_package_manager_missing:
        "未检测到可用包管理器；请按环境检查中的手动命令安装依赖。",
      ca_trust_failed:
        "Relay CA 未能写入系统信任；请在网关页导出 CA 后按系统提示手动信任。",
      codex_cli_missing: "请在设置页运行 Codex 环境检查并安装 Codex CLI。",
      secret_store_unavailable: "请解锁系统安全存储后重试。",
      keychain_interaction_required:
        "请在 macOS 系统弹窗中输入登录钥匙串密码，并选择“始终允许”。",
      profile_credential_migration_required:
        "该档案由旧版应用保存；请使用“更新凭据”重新完成一次 OAuth。",
      profile_runtime_unavailable: "请重新授权该 OAuth 档案后重试。",
      gateway_model_unavailable:
        "请刷新可用模型，并确认对应账号已启用且加入网关账号池。",
      app_update_unavailable: "请检查网络连接，或稍后在设置页手动检查软件更新。",
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
  createApiServiceProfile: (input: Record<string, unknown>) =>
    relayInvoke<MaskedProfile>("create_api_service_profile", { input }),
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
  appUpdateSettings: () => relayInvoke<AppUpdateSettings>("app_update_settings"),
  updateAppUpdateSettings: (settings: AppUpdateSettings) =>
    relayInvoke<AppUpdateSettings>("update_app_update_settings", {
      input: {
        channel: settings.channel,
        auto_check: settings.auto_check,
      },
    }),
  checkAppUpdate: (channel?: AppUpdateChannel | null) =>
    relayInvoke<AppUpdateInfo | null>("check_app_update", {
      input: { channel: channel ?? null },
    }),
  installAppUpdate: (channel?: AppUpdateChannel | null) =>
    relayInvoke<void>("install_app_update", {
      input: { channel: channel ?? null },
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
  exportGatewayCa: (destination: string) =>
    relayInvoke<void>("export_gateway_ca", { destination }),
  trustGatewayCa: () => relayInvoke<void>("trust_gateway_ca"),
  refreshProfileModels: (id: string) =>
    relayInvoke<MaskedProfile>("refresh_profile_models", { id }),
  testApiServiceProfile: (input: Record<string, unknown>) =>
    relayInvoke<ApiServiceTestReport>("test_api_service_profile", { input }),
  testExistingApiServiceProfile: (id: string) =>
    relayInvoke<ApiServiceTestReport>("test_existing_api_service_profile", { id }),
  codexEnvironmentStatus: () =>
    relayInvoke<CodexEnvironmentReport>("codex_environment_status"),
  installCodexEnvironment: (steps?: string[]) =>
    relayInvoke<CodexEnvironmentInstallReport>("install_codex_environment", {
      input: { confirmed: true, steps: steps ?? null },
    }),
  codexGatewayConfigStatus: () =>
    relayInvoke<GatewayCodexConfigStatus>("codex_gateway_config_status"),
  enableCodexGateway: () =>
    relayInvoke<GatewayCodexConfigStatus>("enable_codex_gateway"),
  disableCodexGateway: () =>
    relayInvoke<GatewayCodexConfigStatus>("disable_codex_gateway"),
  setCodexGatewayOAuthProfile: (profileId: string | null) =>
    relayInvoke<GatewayCodexConfigStatus>("set_codex_gateway_oauth_profile", {
      input: { profile_id: profileId, confirmed: true },
    }),
  listGatewayModelOptions: () => relayInvoke<string[]>("list_gateway_model_options"),
  activateApiServiceProfile: (id: string) =>
    relayInvoke<GatewayCodexConfigStatus>("activate_api_service_profile", { id }),
  listClientKeys: () => relayInvoke<MaskedClientKey[]>("list_client_keys"),
  createClientKey: (name: string) =>
    relayInvoke<CreatedClientKey>("create_client_key", {
      input: { name, confirmed: true },
    }),
  revealClientKey: (id: string) =>
    relayInvoke<string>("reveal_client_key", {
      input: { id, confirmed: true },
    }),
  rotateClientKey: (id: string) =>
    relayInvoke<CreatedClientKey>("rotate_client_key", {
      input: { id, confirmed: true },
    }),
  revokeClientKey: (id: string) =>
    relayInvoke<void>("revoke_client_key", { id, confirmed: true }),
  listCollaborationBots: () =>
    relayInvoke<MaskedCollaborationBot[]>("list_collaboration_bots"),
  upsertCollaborationBot: (input: Record<string, unknown>) =>
    relayInvoke<MaskedCollaborationBot>("upsert_collaboration_bot", { input }),
  testCollaborationBot: (id: string) =>
    relayInvoke<MaskedCollaborationBot>("test_collaboration_bot", { id }),
  deleteCollaborationBot: (id: string) =>
    relayInvoke<void>("delete_collaboration_bot", {
      input: { id, confirmed: true },
    }),
  listCollaborationProjectBindings: () =>
    relayInvoke<CollaborationProjectBinding[]>("list_collaboration_project_bindings"),
  upsertCollaborationProjectBinding: (input: Record<string, unknown>) =>
    relayInvoke<CollaborationProjectBinding>("upsert_collaboration_project_binding", {
      input,
    }),
  deleteCollaborationProjectBinding: (id: string) =>
    relayInvoke<void>("delete_collaboration_project_binding", {
      input: { id, confirmed: true },
    }),
  registerDiscordCommands: (id: string) =>
    relayInvoke<MaskedCollaborationBot>("register_discord_commands", { id }),
  collaborationCallbackStatus: () =>
    relayInvoke<CollaborationCallbackStatus>("collaboration_callback_status"),
  listCollaborationContexts: () =>
    relayInvoke<CollaborationContextSummary[]>("list_collaboration_contexts"),
  updateCollaborationContext: (input: Record<string, unknown>) =>
    relayInvoke<CollaborationContextSummary>("update_collaboration_context", {
      input: { ...input, confirmed: true },
    }),
  resetCollaborationContext: (contextId: string) =>
    relayInvoke<CollaborationContextSummary>("reset_collaboration_context", {
      input: { context_id: contextId, confirmed: true },
    }),
  listFeishuBots: () => relayInvoke<MaskedFeishuBot[]>("list_feishu_bots"),
  upsertFeishuBot: (input: Record<string, unknown>) =>
    relayInvoke<MaskedFeishuBot>("upsert_feishu_bot", { input }),
  testFeishuBot: (id: string) =>
    relayInvoke<MaskedFeishuBot>("test_feishu_bot", { id }),
  deleteFeishuBot: (id: string) =>
    relayInvoke<void>("delete_feishu_bot", { input: { id, confirmed: true } }),
  listFeishuProjectBindings: () =>
    relayInvoke<FeishuProjectBinding[]>("list_feishu_project_bindings"),
  upsertFeishuProjectBinding: (input: Record<string, unknown>) =>
    relayInvoke<FeishuProjectBinding>("upsert_feishu_project_binding", { input }),
  deleteFeishuProjectBinding: (id: string) =>
    relayInvoke<void>("delete_feishu_project_binding", {
      input: { id, confirmed: true },
    }),
  listCodexSessions: (bindingId?: string) =>
    relayInvoke<CodexSessionSummary[]>("list_codex_sessions", {
      input: { binding_id: bindingId ?? null },
    }),
  cancelCodexSession: (sessionId: string) =>
    relayInvoke<CodexSessionSummary>("cancel_codex_session", {
      input: { session_id: sessionId, confirmed: true },
    }),
  continueCodexSession: (sessionId: string, instruction: string) =>
    relayInvoke<CodexSessionSummary>("continue_codex_session", {
      input: { session_id: sessionId, instruction, confirmed: true },
    }),
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
  previewJsonProfileImport: (paths: string[]) =>
    relayInvoke<JsonProfileImportPreview>("preview_json_profile_import", {
      input: { paths },
    }),
  commitJsonProfileImport: (previewId: string, itemIds: string[]) =>
    relayInvoke<JsonProfileImportResult>("commit_json_profile_import", {
      input: { preview_id: previewId, item_ids: itemIds },
    }),
  retryJsonProfileImport: (previewId: string) =>
    relayInvoke<JsonProfileImportPreview>("retry_json_profile_import", {
      input: { preview_id: previewId },
    }),
  discardJsonProfileImport: (previewId: string) =>
    relayInvoke<void>("discard_json_profile_import", {
      input: { preview_id: previewId },
    }),
};
