use serde::{Deserialize, Serialize};

pub const GATEWAY_CODEX_CLIENT_KEY_REF_SETTING: &str = "gateway_codex_client_key_ref";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    ApiKey,
    CodexOauth,
}

/// The upstream wire protocol used by a gateway profile. Codex OAuth profiles
/// use the OpenAI Responses-compatible adapter while API-key profiles retain
/// their configured provider protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GatewayProvider {
    OpenAi,
    #[default]
    OpenAiCompatible,
    Anthropic,
    Gemini,
    Ollama,
}

/// The non-secret authentication material used by a Codex profile. The actual
/// credential is always stored in the platform secret store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexAuthMode {
    OAuth,
    AgentIdentity,
    PersonalAccessToken,
}

impl Default for CodexAuthMode {
    fn default() -> Self {
        Self::OAuth
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaskedProfile {
    pub id: String,
    pub alias: String,
    pub kind: ProfileKind,
    pub base_url: Option<String>,
    #[serde(default)]
    pub provider: GatewayProvider,
    pub enabled: bool,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
    pub models: Vec<String>,
    pub health: String,
    pub cooldown_until_ms: Option<i64>,
    pub credential_configured: bool,
    #[serde(default)]
    pub auth_mode: CodexAuthMode,
    pub is_current: bool,
    pub account: Option<ProfileAccountSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewJsonProfileImportInput {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetryJsonProfileImportInput {
    pub preview_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonProfileImportPreview {
    pub preview_id: String,
    pub expires_at_ms: i64,
    pub items: Vec<JsonProfileImportPreviewItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonProfileImportPreviewItem {
    pub id: String,
    pub file_name: String,
    pub alias: String,
    pub auth_mode: CodexAuthMode,
    pub source: String,
    pub status: String,
    pub message: String,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub existing_profile_alias: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitJsonProfileImportInput {
    pub preview_id: String,
    pub item_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscardJsonProfileImportInput {
    pub preview_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonProfileImportResult {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub items: Vec<JsonProfileImportResultItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonProfileImportResultItem {
    pub id: String,
    pub alias: String,
    pub action: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileAccountSummary {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub updated_at_ms: i64,
    pub quota: ProfileQuota,
    pub subscription: ProfileSubscription,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileQuota {
    pub status: String,
    pub message: String,
    pub source: Option<String>,
    pub synced_at_ms: Option<i64>,
    pub last_attempt_at_ms: i64,
    pub last_error: Option<String>,
    pub primary: Option<ProfileQuotaWindow>,
    pub secondary: Option<ProfileQuotaWindow>,
    #[serde(default)]
    pub buckets: Vec<ProfileQuotaBucket>,
    pub rate_limit_reached_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileQuotaWindow {
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub window_duration_mins: i64,
    pub resets_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileQuotaBucket {
    pub id: Option<String>,
    pub name: Option<String>,
    pub plan_type: Option<String>,
    pub primary: Option<ProfileQuotaWindow>,
    pub secondary: Option<ProfileQuotaWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSubscription {
    pub status: String,
    pub plan_type: Option<String>,
    pub period_ends_at_ms: Option<i64>,
    pub will_renew: Option<bool>,
    pub source: Option<String>,
    pub synced_at_ms: Option<i64>,
    pub last_attempt_at_ms: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileQuotaRefreshReport {
    pub profiles: Vec<MaskedProfile>,
    pub failed_profile_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateProfileInput {
    pub alias: String,
    pub kind: ProfileKind,
    pub base_url: Option<String>,
    #[serde(default)]
    pub provider: GatewayProvider,
    pub api_key: Option<String>,
    pub models: Vec<String>,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestApiServiceInput {
    pub provider: GatewayProvider,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServiceTestReport {
    pub status: String,
    pub category: String,
    pub endpoint: String,
    pub message: String,
    pub http_status: Option<u16>,
    pub latency_ms: i64,
    pub model_count: usize,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateProfileInput {
    pub id: String,
    pub alias: String,
    pub enabled: bool,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
    pub models: Vec<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub gateway: GatewayStatus,
    pub profiles: Vec<MaskedProfile>,
    pub metrics: MetricsSnapshot,
    pub workspace_mode: DesktopWorkspaceMode,
    pub collaboration: CollaborationSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationSummary {
    pub enabled_bots: i64,
    pub bound_chats: i64,
    pub active_sessions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub total_requests: i64,
    pub successful_requests: i64,
    pub failed_requests: i64,
    pub average_latency_ms: Option<i64>,
    pub estimated_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub running: bool,
    pub bind_mode: String,
    pub bind_address: String,
    pub available_addresses: Vec<GatewayNetworkAddress>,
    pub port: u16,
    pub cidrs: Vec<String>,
    pub available_profiles: usize,
    pub cooling_profiles: usize,
    pub client_key_count: usize,
    pub certificate_ready: bool,
    pub service_url: String,
    pub upstream_proxy_mode: String,
    pub upstream_proxy_display: Option<String>,
    pub upstream_last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayNetworkAddress {
    pub name: String,
    pub address: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayCodexConfigStatus {
    pub enabled: bool,
    pub config_path: String,
    pub service_url: Option<String>,
    pub message: String,
    pub auth_status: String,
    pub needs_repair: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateGatewayInput {
    pub bind_mode: String,
    pub bind_address: String,
    pub port: u16,
    pub cidrs: Vec<String>,
    pub confirmed_lan: bool,
    pub upstream_proxy_mode: Option<String>,
    pub upstream_proxy_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MaskedClientKey {
    pub id: String,
    pub name: String,
    pub masked_value: String,
    pub created_at_ms: i64,
    pub last_used_at_ms: Option<i64>,
    pub revoked: bool,
    pub managed_by: String,
    pub can_revoke: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedClientKey {
    pub key: MaskedClientKey,
    pub plaintext_once: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateClientKeyInput {
    pub name: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationProvider {
    Feishu,
    Qq,
    Wecom,
    Discord,
    Telegram,
}

#[derive(Debug, Clone, Serialize)]
pub struct MaskedCollaborationBot {
    pub id: String,
    pub provider: CollaborationProvider,
    pub name: String,
    pub enabled: bool,
    pub connection_status: String,
    pub credential_mask: String,
    pub config_summary: String,
    pub callback_public_url: Option<String>,
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertCollaborationBotInput {
    pub id: Option<String>,
    pub provider: CollaborationProvider,
    pub name: String,
    pub enabled: bool,
    pub confirmed: bool,
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    pub client_secret: Option<String>,
    pub corp_id: Option<String>,
    pub agent_id: Option<String>,
    pub secret: Option<String>,
    pub token: Option<String>,
    pub encoding_aes_key: Option<String>,
    pub callback_public_url: Option<String>,
    pub application_id: Option<String>,
    pub bot_token: Option<String>,
    pub guild_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteCollaborationBotInput {
    pub id: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollaborationProjectBinding {
    pub id: String,
    pub provider: CollaborationProvider,
    pub bot_id: String,
    pub bot_name: String,
    pub project_name: String,
    pub project_slug: String,
    pub working_directory: String,
    pub profile_id: String,
    pub profile_alias: String,
    pub chat_id: Option<String>,
    pub bind_code: String,
    pub enabled: bool,
    pub concurrency_limit: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertCollaborationProjectBindingInput {
    pub id: Option<String>,
    pub bot_id: String,
    pub project_name: String,
    pub project_slug: String,
    pub working_directory: String,
    pub profile_id: String,
    pub enabled: bool,
    pub concurrency_limit: i64,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteCollaborationProjectBindingInput {
    pub id: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollaborationCommandResult {
    pub status: String,
    pub message: String,
    pub session: Option<CodexSessionSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollaborationCallbackStatus {
    pub local_url: String,
    pub public_urls: Vec<String>,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MaskedFeishuBot {
    pub id: String,
    pub name: String,
    pub app_id: String,
    pub app_id_mask: String,
    pub enabled: bool,
    pub connection_status: String,
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertFeishuBotInput {
    pub id: Option<String>,
    pub name: String,
    pub app_id: String,
    pub app_secret: Option<String>,
    pub enabled: bool,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteFeishuBotInput {
    pub id: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeishuProjectBinding {
    pub id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub project_name: String,
    pub project_slug: String,
    pub working_directory: String,
    pub profile_id: String,
    pub profile_alias: String,
    pub chat_id: Option<String>,
    pub bind_code: String,
    pub enabled: bool,
    pub concurrency_limit: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertFeishuProjectBindingInput {
    pub id: Option<String>,
    pub bot_id: String,
    pub project_name: String,
    pub project_slug: String,
    pub working_directory: String,
    pub profile_id: String,
    pub enabled: bool,
    pub concurrency_limit: i64,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteFeishuProjectBindingInput {
    pub id: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexSessionSummary {
    pub id: String,
    pub binding_id: String,
    pub provider: CollaborationProvider,
    pub provider_bot_id: Option<String>,
    pub provider_chat_id: Option<String>,
    pub provider_message_id: Option<String>,
    pub project_name: String,
    pub project_slug: String,
    pub profile_id: String,
    pub profile_alias: String,
    pub relay_status: String,
    pub codex_session_id: Option<String>,
    pub feishu_message_id: Option<String>,
    pub feishu_chat_id: Option<String>,
    pub started_by: Option<String>,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub summary: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexSessionEvent {
    pub id: String,
    pub session_id: String,
    pub occurred_at_ms: i64,
    pub event_type: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListCodexSessionsInput {
    pub binding_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelCodexSessionInput {
    pub session_id: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContinueCodexSessionInput {
    pub session_id: String,
    pub instruction: String,
    pub confirmed: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct FeishuCommandResult {
    pub status: String,
    pub message: String,
    pub session: Option<CodexSessionSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OAuthImportStatus {
    pub attempt_id: String,
    pub profile_id: Option<String>,
    pub phase: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartOAuthImportInput {
    pub profile_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompleteOAuthImportInput {
    pub attempt_id: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentProfileActivation {
    pub profile_id: String,
    pub attempt_id: Option<String>,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelectCurrentProfileInput {
    pub id: String,
    pub confirmed_desktop_restart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesktopWorkspaceMode {
    Fresh,
    PerProfile,
    Shared,
}

#[derive(Debug, Clone, Serialize)]
pub struct DesktopWorkspaceSettings {
    pub mode: DesktopWorkspaceMode,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateDesktopWorkspaceSettingsInput {
    pub mode: DesktopWorkspaceMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct DesktopWorkspaceHistoryItem {
    pub id: String,
    pub profile_id: String,
    pub profile_alias: String,
    pub created_at_ms: i64,
    pub last_launched_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestoreDesktopWorkspaceInput {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteDesktopWorkspaceInput {
    pub id: String,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurrentProfileActivationStatusInput {
    pub attempt_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartManagedTaskInput {
    pub instruction: String,
    pub working_directory: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelManagedTaskInput {
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedTaskStatus {
    pub phase: String,
    pub profile_id: Option<String>,
    pub message: String,
}
