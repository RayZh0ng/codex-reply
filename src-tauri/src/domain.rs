use serde::{Deserialize, Serialize};

pub const GATEWAY_CODEX_CLIENT_KEY_REF_SETTING: &str = "gateway_codex_client_key_ref";
pub const GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING: &str = "gateway_codex_oauth_profile_id";
pub const APP_UPDATE_STABLE_ENDPOINT: &str =
    "https://github.com/RayZh0ng/codex-reply/releases/download/updater/stable.json";
pub const APP_UPDATE_BETA_ENDPOINT: &str =
    "https://github.com/RayZh0ng/codex-reply/releases/download/updater/beta.json";

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
pub enum GatewayProvider {
    #[serde(rename = "openai", alias = "open_ai")]
    OpenAi,
    #[default]
    #[serde(rename = "openai_compatible", alias = "open_ai_compatible")]
    OpenAiCompatible,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "ollama")]
    Ollama,
}

/// The upstream API shape used by an API-key profile. Codex always talks to
/// Relay through an OpenAI Responses-compatible surface; chat-completions
/// upstreams are converted by the local Relay gateway before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GatewayWireApi {
    #[default]
    Responses,
    ChatCompletions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayModelMapping {
    /// Model id exposed by Relay and shown to Codex/client callers.
    pub model: String,
    /// Real model id sent to the upstream provider.
    pub upstream_model: String,
    /// Optional display name for generated Codex model catalogs.
    pub display_name: Option<String>,
    /// Optional context window for generated Codex model catalogs.
    pub context_window: Option<i64>,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppUpdateChannel {
    Stable,
    Beta,
}

impl AppUpdateChannel {
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Stable => APP_UPDATE_STABLE_ENDPOINT,
            Self::Beta => APP_UPDATE_BETA_ENDPOINT,
        }
    }

    pub fn as_setting_value(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppUpdateSettings {
    pub channel: AppUpdateChannel,
    pub auto_check: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateAppUpdateSettingsInput {
    pub channel: AppUpdateChannel,
    pub auto_check: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckAppUpdateInput {
    #[serde(default)]
    pub channel: Option<AppUpdateChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallAppUpdateInput {
    #[serde(default)]
    pub channel: Option<AppUpdateChannel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppUpdateInfo {
    pub version: String,
    pub current_version: String,
    pub body: Option<String>,
    pub date: Option<String>,
    pub channel: AppUpdateChannel,
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
    #[serde(default)]
    pub wire_api: GatewayWireApi,
    pub enabled: bool,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
    pub models: Vec<String>,
    #[serde(default)]
    pub model_mappings: Vec<GatewayModelMapping>,
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
    pub profile_id: Option<String>,
    pub auth_mode: Option<CodexAuthMode>,
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
    #[serde(default)]
    pub wire_api: GatewayWireApi,
    pub api_key: Option<String>,
    pub models: Vec<String>,
    #[serde(default)]
    pub model_mappings: Vec<GatewayModelMapping>,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateApiServiceProfileInput {
    pub alias: String,
    #[serde(default)]
    pub provider: GatewayProvider,
    #[serde(default)]
    pub wire_api: GatewayWireApi,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub model_mappings: Vec<GatewayModelMapping>,
    #[serde(default)]
    pub in_pool: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_profile_weight")]
    pub weight: i64,
}

fn default_profile_weight() -> i64 {
    1
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
    #[serde(default)]
    pub model_mappings: Option<Vec<GatewayModelMapping>>,
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
    pub oauth_profile_id: Option<String>,
    pub oauth_profile_alias: Option<String>,
    pub oauth_profile_available: bool,
    pub oauth_profile_options: Vec<GatewayOAuthProfileOption>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayOAuthProfileOption {
    pub id: String,
    pub alias: String,
    pub available: bool,
    pub reason: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct SetCodexGatewayOAuthProfileInput {
    pub profile_id: Option<String>,
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
    pub system_prompt: Option<String>,
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
    #[serde(default)]
    pub system_prompt: Option<String>,
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
    pub profile_id: Option<String>,
    pub profile_alias: Option<String>,
    pub chat_id: Option<String>,
    pub bind_code: String,
    pub enabled: bool,
    pub concurrency_limit: i64,
    pub execution_target: String,
    pub model_id: Option<String>,
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
    pub profile_id: Option<String>,
    pub enabled: bool,
    pub concurrency_limit: i64,
    #[serde(default)]
    pub execution_target: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
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
    pub profile_id: Option<String>,
    pub profile_alias: Option<String>,
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
    pub execution_target: String,
    pub model_id: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::{
        AppUpdateChannel, GatewayProvider, APP_UPDATE_BETA_ENDPOINT, APP_UPDATE_STABLE_ENDPOINT,
    };

    #[test]
    fn gateway_provider_accepts_legacy_and_frontend_spellings() {
        assert_eq!(
            serde_json::from_str::<GatewayProvider>("\"openai_compatible\"").unwrap(),
            GatewayProvider::OpenAiCompatible
        );
        assert_eq!(
            serde_json::from_str::<GatewayProvider>("\"open_ai_compatible\"").unwrap(),
            GatewayProvider::OpenAiCompatible
        );
        assert_eq!(
            serde_json::to_string(&GatewayProvider::OpenAiCompatible).unwrap(),
            "\"openai_compatible\""
        );
    }

    #[test]
    fn app_update_channels_map_to_fixed_github_manifest_endpoints() {
        assert_eq!(
            AppUpdateChannel::Stable.endpoint(),
            APP_UPDATE_STABLE_ENDPOINT
        );
        assert_eq!(AppUpdateChannel::Beta.endpoint(), APP_UPDATE_BETA_ENDPOINT);
        assert!(APP_UPDATE_STABLE_ENDPOINT.ends_with("/stable.json"));
        assert!(APP_UPDATE_BETA_ENDPOINT.ends_with("/beta.json"));
        assert!(serde_json::from_str::<AppUpdateChannel>("\"nightly\"").is_err());
    }
}
