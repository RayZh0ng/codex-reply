use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    ApiKey,
    CodexOauth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaskedProfile {
    pub id: String,
    pub alias: String,
    pub kind: ProfileKind,
    pub base_url: Option<String>,
    pub enabled: bool,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
    pub models: Vec<String>,
    pub health: String,
    pub cooldown_until_ms: Option<i64>,
    pub credential_configured: bool,
    pub is_current: bool,
    pub account: Option<ProfileAccountSummary>,
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
    pub api_key: Option<String>,
    pub models: Vec<String>,
    pub in_pool: bool,
    pub priority: i64,
    pub weight: i64,
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
    pub notifications: Vec<MaskedChannel>,
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
    pub port: u16,
    pub cidrs: Vec<String>,
    pub available_profiles: usize,
    pub cooling_profiles: usize,
    pub client_key_count: usize,
    pub certificate_ready: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateGatewayInput {
    pub bind_mode: String,
    pub bind_address: String,
    pub port: u16,
    pub cidrs: Vec<String>,
    pub confirmed_lan: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MaskedClientKey {
    pub id: String,
    pub name: String,
    pub masked_value: String,
    pub created_at_ms: i64,
    pub last_used_at_ms: Option<i64>,
    pub revoked: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Feishu,
    Wecom,
    Custom,
}

#[derive(Debug, Clone, Serialize)]
pub struct MaskedChannel {
    pub id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub enabled: bool,
    pub endpoint_mask: String,
    pub last_status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertChannelInput {
    pub id: Option<String>,
    pub name: String,
    pub kind: ChannelKind,
    pub endpoint: String,
    pub signing_secret: Option<String>,
    pub enabled: bool,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestChannelInput {
    pub id: String,
    pub confirmed: bool,
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
