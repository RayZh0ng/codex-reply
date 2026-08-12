use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::{
    domain::{
        AppUpdateChannel, AppUpdateSettings, CodexAuthMode, CodexSessionEvent, CodexSessionSummary,
        CollaborationContextSummary, CollaborationProjectBinding, CollaborationProvider,
        CollaborationSummary, DesktopWorkspaceHistoryItem, DesktopWorkspaceMode,
        FeishuProjectBinding, GatewayDirectRouteHealth, GatewayModelMapping, GatewayNetworkAddress,
        GatewayPerformanceInput, GatewayProvider, GatewayRequestMetricPage,
        GatewayRequestMetricSummary, GatewayStatus, GatewayWireApi, ListGatewayRequestMetricsInput,
        MaskedClientKey, MaskedCollaborationBot, MaskedFeishuBot, MaskedProfile, MetricsSnapshot,
        ProfileAccountSummary, ProfileKind, ProfileQuota, ProfileSubscription,
        GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING,
        GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
    },
    error::{AppError, AppResult},
};

pub struct Repository {
    connection: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct StoredProfile {
    pub profile: MaskedProfile,
    pub secret_ref: Option<String>,
    pub credential_fingerprint: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StoredFeishuBot {
    pub bot: MaskedFeishuBot,
    pub app_secret_ref: String,
}

#[derive(Debug, Clone)]
pub struct StoredCollaborationBot {
    pub bot: MaskedCollaborationBot,
    pub config_json: String,
    pub secret_refs_json: String,
}

#[derive(Debug, Clone)]
pub struct StoredCodexSession {
    pub session: CodexSessionSummary,
    pub working_directory: String,
}

#[derive(Debug, Clone)]
pub struct StoredDesktopWorkspace {
    pub id: String,
    pub profile_id: String,
}

impl Repository {
    pub fn open(path: &Path) -> AppResult<Self> {
        let connection = Connection::open(path).map_err(|_| AppError::Internal)?;
        let repository = Self {
            connection: Mutex::new(connection),
        };
        repository.migrate()?;
        Ok(repository)
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        let connection = Connection::open_in_memory().expect("in-memory sqlite must open");
        let repository = Self {
            connection: Mutex::new(connection),
        };
        repository.migrate().expect("migration must work");
        repository
    }

    fn with_connection<T>(&self, action: impl FnOnce(&Connection) -> AppResult<T>) -> AppResult<T> {
        let connection = self.connection.lock().map_err(|_| AppError::Internal)?;
        action(&connection)
    }

    fn migrate(&self) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute_batch(
                    "
                    PRAGMA foreign_keys = ON;
                    CREATE TABLE IF NOT EXISTS profiles (
                      id TEXT PRIMARY KEY,
                      alias TEXT NOT NULL UNIQUE,
                      kind TEXT NOT NULL,
                      base_url TEXT,
                      provider TEXT NOT NULL DEFAULT 'openai_compatible',
                      wire_api TEXT NOT NULL DEFAULT 'responses',
                      enabled INTEGER NOT NULL,
                      in_pool INTEGER NOT NULL,
                      priority INTEGER NOT NULL,
                      weight INTEGER NOT NULL,
                      models_json TEXT NOT NULL,
                      model_mappings_json TEXT NOT NULL DEFAULT '[]',
                      codex_oauth_profile_id TEXT,
                      health TEXT NOT NULL,
                      cooldown_until_ms INTEGER,
                      secret_ref TEXT,
                      credential_configured INTEGER NOT NULL DEFAULT 0,
                      auth_mode TEXT NOT NULL DEFAULT 'oauth',
                      credential_fingerprint TEXT,
                      account_display_name TEXT,
                      account_email TEXT,
                      account_id TEXT,
                      account_updated_at_ms INTEGER,
                      account_quota_json TEXT,
                      account_subscription_json TEXT,
                      validation_status TEXT NOT NULL DEFAULT 'unknown',
                      validated_at_ms INTEGER,
                      validation_message TEXT
                    );
                    CREATE TABLE IF NOT EXISTS app_settings (
                      key TEXT PRIMARY KEY,
                      value TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS desktop_workspaces (
                      id TEXT PRIMARY KEY,
                      profile_id TEXT,
                      created_at_ms INTEGER NOT NULL,
                      last_launched_at_ms INTEGER NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS gateway_settings (
                      id INTEGER PRIMARY KEY CHECK(id = 1),
                      bind_mode TEXT NOT NULL,
                      bind_address TEXT NOT NULL,
                      port INTEGER NOT NULL,
                      cidrs_json TEXT NOT NULL
                    );
                    INSERT OR IGNORE INTO gateway_settings(id, bind_mode, bind_address, port, cidrs_json)
                    VALUES (1, 'loopback', '127.0.0.1', 53765, '[]');
                    CREATE TABLE IF NOT EXISTS client_keys (
                      id TEXT PRIMARY KEY,
                      name TEXT NOT NULL,
                      key_hash TEXT NOT NULL,
                      masked_value TEXT NOT NULL,
                      created_at_ms INTEGER NOT NULL,
                      last_used_at_ms INTEGER,
                      revoked INTEGER NOT NULL,
                      secret_ref TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS feishu_bots (
                      id TEXT PRIMARY KEY,
                      name TEXT NOT NULL,
                      app_id TEXT NOT NULL,
                      app_id_mask TEXT NOT NULL,
                      app_secret_ref TEXT NOT NULL,
                      enabled INTEGER NOT NULL,
                      connection_status TEXT NOT NULL,
                      last_error TEXT,
                      updated_at_ms INTEGER NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS feishu_project_bindings (
                      id TEXT PRIMARY KEY,
                      bot_id TEXT NOT NULL,
                      project_name TEXT NOT NULL,
                      project_slug TEXT NOT NULL,
                      working_directory TEXT NOT NULL,
                      profile_id TEXT NOT NULL,
                      chat_id TEXT,
                      bind_code TEXT NOT NULL UNIQUE,
                      enabled INTEGER NOT NULL,
                      concurrency_limit INTEGER NOT NULL,
                      created_at_ms INTEGER NOT NULL,
                      updated_at_ms INTEGER NOT NULL,
                      FOREIGN KEY(bot_id) REFERENCES feishu_bots(id) ON DELETE CASCADE,
                      FOREIGN KEY(profile_id) REFERENCES profiles(id) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_feishu_project_bindings_bot_chat
                      ON feishu_project_bindings(bot_id, chat_id);
                    CREATE TABLE IF NOT EXISTS collaboration_bots (
                      id TEXT PRIMARY KEY,
                      provider TEXT NOT NULL,
                      name TEXT NOT NULL,
                      enabled INTEGER NOT NULL,
                      credential_mask TEXT NOT NULL,
                      config_summary TEXT NOT NULL,
                      config_json TEXT NOT NULL,
                      secret_refs_json TEXT NOT NULL,
                      connection_status TEXT NOT NULL,
                      last_error TEXT,
                      callback_public_url TEXT,
                      updated_at_ms INTEGER NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_collaboration_bots_provider
                      ON collaboration_bots(provider);
                    CREATE TABLE IF NOT EXISTS collaboration_project_bindings (
                      id TEXT PRIMARY KEY,
                      provider TEXT NOT NULL,
                      bot_id TEXT NOT NULL,
                      project_name TEXT NOT NULL,
                      project_slug TEXT NOT NULL,
                      working_directory TEXT NOT NULL,
                      profile_id TEXT,
                      chat_id TEXT,
                      bind_code TEXT NOT NULL UNIQUE,
                      enabled INTEGER NOT NULL,
                      concurrency_limit INTEGER NOT NULL,
                      execution_target TEXT NOT NULL DEFAULT 'profile',
                      model_id TEXT,
                      created_at_ms INTEGER NOT NULL,
                      updated_at_ms INTEGER NOT NULL,
                      FOREIGN KEY(bot_id) REFERENCES collaboration_bots(id) ON DELETE CASCADE,
                      FOREIGN KEY(profile_id) REFERENCES profiles(id) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_collaboration_project_bindings_bot_chat
                      ON collaboration_project_bindings(provider, bot_id, chat_id);
                    CREATE TABLE IF NOT EXISTS collaboration_provider_state (
                      provider TEXT NOT NULL,
                      bot_id TEXT NOT NULL,
                      state_key TEXT NOT NULL,
                      state_value TEXT NOT NULL,
                      updated_at_ms INTEGER NOT NULL,
                      PRIMARY KEY(provider, bot_id, state_key)
                    );
                    CREATE TABLE IF NOT EXISTS collaboration_contexts (
                      id TEXT PRIMARY KEY,
                      scope_key TEXT NOT NULL UNIQUE,
                      binding_id TEXT NOT NULL,
                      memory_enabled INTEGER NOT NULL DEFAULT 1,
                      permissions_policy TEXT NOT NULL DEFAULT 'workspace-write',
                      model_id TEXT,
                      active_codex_session_id TEXT,
                      active_relay_session_id TEXT,
                      goal_status TEXT NOT NULL DEFAULT 'none',
                      goal_text TEXT,
                      conversation_mode TEXT NOT NULL DEFAULT 'default',
                      last_turn_at_ms INTEGER,
                      created_at_ms INTEGER NOT NULL,
                      updated_at_ms INTEGER NOT NULL,
                      FOREIGN KEY(binding_id) REFERENCES collaboration_project_bindings(id) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_collaboration_contexts_binding
                      ON collaboration_contexts(binding_id);
                    CREATE TABLE IF NOT EXISTS collaboration_chat_state (
                      provider TEXT NOT NULL,
                      bot_id TEXT NOT NULL,
                      chat_id TEXT NOT NULL,
                      context_id TEXT NOT NULL,
                      updated_at_ms INTEGER NOT NULL,
                      PRIMARY KEY(provider, bot_id, chat_id),
                      FOREIGN KEY(context_id) REFERENCES collaboration_contexts(id) ON DELETE CASCADE
                    );
                    CREATE TABLE IF NOT EXISTS codex_sessions (
                      id TEXT PRIMARY KEY,
                      binding_id TEXT NOT NULL,
                      context_id TEXT,
                      profile_id TEXT,
                      provider TEXT NOT NULL DEFAULT 'feishu',
                      provider_bot_id TEXT,
                      provider_chat_id TEXT,
                      provider_message_id TEXT,
                      relay_status TEXT NOT NULL,
                      codex_session_id TEXT,
                      feishu_message_id TEXT,
                      feishu_chat_id TEXT,
                      started_by TEXT,
                      started_at_ms INTEGER NOT NULL,
                      updated_at_ms INTEGER NOT NULL,
                      finished_at_ms INTEGER,
                      summary TEXT,
                      last_error TEXT,
                      execution_target TEXT NOT NULL DEFAULT 'profile',
                      model_id TEXT,
                      turn_kind TEXT NOT NULL DEFAULT 'run',
                      conversation_mode TEXT NOT NULL DEFAULT 'default',
                      goal_status TEXT,
                      working_directory TEXT NOT NULL,
                      FOREIGN KEY(binding_id) REFERENCES collaboration_project_bindings(id) ON DELETE CASCADE,
                      FOREIGN KEY(context_id) REFERENCES collaboration_contexts(id) ON DELETE SET NULL,
                      FOREIGN KEY(profile_id) REFERENCES profiles(id) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_codex_sessions_binding_status
                      ON codex_sessions(binding_id, relay_status);
                    CREATE TABLE IF NOT EXISTS codex_session_events (
                      id TEXT PRIMARY KEY,
                      session_id TEXT NOT NULL,
                      occurred_at_ms INTEGER NOT NULL,
                      event_type TEXT NOT NULL,
                      content TEXT NOT NULL,
                      FOREIGN KEY(session_id) REFERENCES codex_sessions(id) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_codex_session_events_session_time
                      ON codex_session_events(session_id, occurred_at_ms);
                    CREATE TABLE IF NOT EXISTS metrics (
                      id INTEGER PRIMARY KEY CHECK(id = 1),
                      total_requests INTEGER NOT NULL DEFAULT 0,
                      successful_requests INTEGER NOT NULL DEFAULT 0,
                      failed_requests INTEGER NOT NULL DEFAULT 0,
                      total_latency_ms INTEGER NOT NULL DEFAULT 0,
                      latency_samples INTEGER NOT NULL DEFAULT 0,
                      estimated_tokens INTEGER NOT NULL DEFAULT 0
                    );
                    INSERT OR IGNORE INTO metrics(id) VALUES (1);
                    CREATE TABLE IF NOT EXISTS gateway_request_metrics (
                      sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                      request_id TEXT NOT NULL UNIQUE,
                      started_at_ms INTEGER NOT NULL,
                      route TEXT NOT NULL, provider TEXT NOT NULL, profile_id TEXT, auth_mode TEXT NOT NULL,
                      stream INTEGER NOT NULL, auth_latency_ms INTEGER NOT NULL, queue_latency_ms INTEGER NOT NULL,
                      ttfb_ms INTEGER, total_latency_ms INTEGER NOT NULL, request_bytes INTEGER NOT NULL,
                      response_bytes INTEGER NOT NULL, http_status INTEGER NOT NULL, outcome TEXT NOT NULL,
                      error_category TEXT, upstream_attempts INTEGER NOT NULL, retry_count INTEGER NOT NULL,
                      input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, total_tokens INTEGER NOT NULL,
                      upstream_response_id TEXT
                    );
                    CREATE INDEX IF NOT EXISTS idx_gateway_request_metrics_started ON gateway_request_metrics(started_at_ms DESC);
                    CREATE INDEX IF NOT EXISTS idx_gateway_request_metrics_profile_started ON gateway_request_metrics(profile_id, started_at_ms DESC);
                    ",
                )
                .map_err(|_| AppError::Internal)?;
            let profile_columns = profile_columns(connection)?;
            let has_credential_configured = profile_columns
                .iter()
                .any(|name| name == "credential_configured");
            if !has_credential_configured {
                connection
                    .execute("ALTER TABLE profiles ADD COLUMN credential_configured INTEGER NOT NULL DEFAULT 0", [])
                    .map_err(|_| AppError::Internal)?;
                connection
                    .execute("UPDATE profiles SET credential_configured = CASE WHEN secret_ref IS NULL THEN 0 ELSE 1 END", [])
                    .map_err(|_| AppError::Internal)?;
            }
            for (name, definition) in [
                ("auth_mode", "TEXT NOT NULL DEFAULT 'oauth'"),
                ("credential_fingerprint", "TEXT"),
                ("account_display_name", "TEXT"),
                ("account_email", "TEXT"),
                ("account_id", "TEXT"),
                ("account_updated_at_ms", "INTEGER"),
                ("account_quota_json", "TEXT"),
                ("account_subscription_json", "TEXT"),
                ("provider", "TEXT NOT NULL DEFAULT 'openai_compatible'"),
                ("wire_api", "TEXT NOT NULL DEFAULT 'responses'"),
                ("model_mappings_json", "TEXT NOT NULL DEFAULT '[]'"),
                ("max_concurrency", "INTEGER NOT NULL DEFAULT 4"),
                ("max_queue_depth", "INTEGER NOT NULL DEFAULT 8"),
                ("queue_timeout_ms", "INTEGER NOT NULL DEFAULT 15000"),
                ("codex_oauth_profile_id", "TEXT"),
                ("validation_status", "TEXT NOT NULL DEFAULT 'unknown'"),
                ("validated_at_ms", "INTEGER"),
                ("validation_message", "TEXT"),
            ] {
                if !profile_columns.iter().any(|column| column == name) {
                    connection
                        .execute(
                            &format!("ALTER TABLE profiles ADD COLUMN {name} {definition}"),
                            [],
                        )
                        .map_err(|_| AppError::Internal)?;
                }
            }
            let binding_columns = table_columns(connection, "collaboration_project_bindings")?;
            for (name, definition) in [
                ("execution_target", "TEXT NOT NULL DEFAULT 'profile'"),
                ("model_id", "TEXT"),
            ] {
                if !binding_columns.iter().any(|column| column == name) {
                    connection
                        .execute(
                            &format!(
                                "ALTER TABLE collaboration_project_bindings ADD COLUMN {name} {definition}"
                            ),
                            [],
                        )
                        .map_err(|_| AppError::Internal)?;
                }
            }
            let session_columns = table_columns(connection, "codex_sessions")?;
            for (name, definition) in [
                ("provider", "TEXT NOT NULL DEFAULT 'feishu'"),
                ("provider_bot_id", "TEXT"),
                ("provider_chat_id", "TEXT"),
                ("provider_message_id", "TEXT"),
                ("context_id", "TEXT"),
                ("execution_target", "TEXT NOT NULL DEFAULT 'profile'"),
                ("model_id", "TEXT"),
                ("turn_kind", "TEXT NOT NULL DEFAULT 'run'"),
                ("conversation_mode", "TEXT NOT NULL DEFAULT 'default'"),
                ("goal_status", "TEXT"),
            ] {
                if !session_columns.iter().any(|column| column == name) {
                    connection
                        .execute(
                            &format!("ALTER TABLE codex_sessions ADD COLUMN {name} {definition}"),
                            [],
                        )
                    .map_err(|_| AppError::Internal)?;
                }
            }
            let context_columns = table_columns(connection, "collaboration_contexts")?;
            for (name, definition) in [
                ("permissions_policy", "TEXT NOT NULL DEFAULT 'workspace-write'"),
                ("model_id", "TEXT"),
            ] {
                if !context_columns.iter().any(|column| column == name) {
                    connection
                        .execute(
                            &format!("ALTER TABLE collaboration_contexts ADD COLUMN {name} {definition}"),
                            [],
                        )
                        .map_err(|_| AppError::Internal)?;
                }
            }
            make_collaboration_profile_columns_nullable(connection)?;
            connection
                .execute(
                    "CREATE INDEX IF NOT EXISTS idx_codex_sessions_context_status ON codex_sessions(context_id, relay_status)",
                    [],
                )
                .map_err(|_| AppError::Internal)?;
            migrate_feishu_to_collaboration(connection)?;
            Ok(())
        })
    }

    pub fn insert_profile(&self, stored: &StoredProfile) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO profiles(id, alias, kind, base_url, provider, wire_api, enabled, in_pool, priority, weight, max_concurrency, max_queue_depth, queue_timeout_ms, models_json, model_mappings_json, codex_oauth_profile_id, health, cooldown_until_ms, secret_ref, credential_configured, auth_mode, credential_fingerprint, account_display_name, account_email, account_id, account_updated_at_ms, account_quota_json, account_subscription_json, validation_status, validated_at_ms, validation_message)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31)",
                    params![
                        stored.profile.id,
                        stored.profile.alias,
                        profile_kind_name(&stored.profile.kind),
                        stored.profile.base_url,
                        provider_name(&stored.profile.provider),
                        wire_api_name(&stored.profile.wire_api),
                        stored.profile.enabled,
                        stored.profile.in_pool,
                        stored.profile.priority,
                        stored.profile.weight,
                        stored.profile.max_concurrency,
                        stored.profile.max_queue_depth,
                        stored.profile.queue_timeout_ms,
                        serde_json::to_string(&stored.profile.models).map_err(|_| AppError::Internal)?,
                        serde_json::to_string(&stored.profile.model_mappings).map_err(|_| AppError::Internal)?,
                        stored.profile.codex_oauth_profile_id,
                        stored.profile.health,
                        stored.profile.cooldown_until_ms,
                        stored.secret_ref,
                        stored.profile.credential_configured,
                        auth_mode_name(&stored.profile.auth_mode),
                        stored.credential_fingerprint,
                        stored.profile.account.as_ref().and_then(|account| account.display_name.clone()),
                        stored.profile.account.as_ref().and_then(|account| account.email.clone()),
                        stored.profile.account.as_ref().and_then(|account| account.account_id.clone()),
                        stored.profile.account.as_ref().map(|account| account.updated_at_ms),
                        stored.profile.account.as_ref().and_then(|account| serde_json::to_string(&account.quota).ok()),
                        stored.profile.account.as_ref().and_then(|account| serde_json::to_string(&account.subscription).ok()),
                        stored.profile.validation_status,
                        stored.profile.validated_at_ms,
                        stored.profile.validation_message,
                    ],
                )
                .map_err(|error| match error {
                    rusqlite::Error::SqliteFailure(_, _) => AppError::Conflict,
                    _ => AppError::Internal,
                })?;
            Ok(())
        })
    }

    pub fn list_profiles(&self) -> AppResult<Vec<StoredProfile>> {
        self.with_connection(|connection| {
            let current_id = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = 'current_profile'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let mut statement = connection
                .prepare("SELECT id, alias, kind, base_url, provider, wire_api, enabled, in_pool, priority, weight, max_concurrency, max_queue_depth, queue_timeout_ms, models_json, model_mappings_json, codex_oauth_profile_id, health, cooldown_until_ms, secret_ref, credential_configured, auth_mode, credential_fingerprint, account_display_name, account_email, account_id, account_updated_at_ms, account_quota_json, account_subscription_json, validation_status, validated_at_ms, validation_message FROM profiles ORDER BY priority, alias")
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let kind: String = row.get(2)?;
                    let models_json: String = row.get(13)?;
                    let models: Vec<String> = serde_json::from_str(&models_json).unwrap_or_default();
                    let mappings_json: String = row.get(14)?;
                    let parsed_mappings =
                        serde_json::from_str(&mappings_json).unwrap_or_default();
                    let model_mappings = stored_or_identity_model_mappings(&models, parsed_mappings);
                    Ok(StoredProfile {
                        secret_ref: row.get(18)?,
                        credential_fingerprint: row.get(21)?,
                        profile: MaskedProfile {
                            id: id.clone(),
                            alias: row.get(1)?,
                            kind: parse_profile_kind(&kind).unwrap_or(ProfileKind::ApiKey),
                            base_url: row.get(3)?,
                            provider: parse_provider(&row.get::<_, String>(4)?).unwrap_or_default(),
                            wire_api: parse_wire_api(&row.get::<_, String>(5)?).unwrap_or_default(),
                            enabled: row.get(6)?,
                            in_pool: row.get(7)?,
                            priority: row.get(8)?,
                            weight: row.get(9)?,
                            max_concurrency: row.get(10)?,
                            max_queue_depth: row.get(11)?,
                            queue_timeout_ms: row.get(12)?,
                            models,
                            model_mappings,
                            codex_oauth_profile_id: row.get(15)?,
                            health: row.get(16)?,
                            cooldown_until_ms: row.get(17)?,
                            credential_configured: row.get(19)?,
                            auth_mode: parse_auth_mode(&row.get::<_, String>(20)?)
                                .unwrap_or_default(),
                            is_current: current_id.as_deref() == Some(id.as_str()),
                            account: account_summary(
                                row.get(22)?,
                                row.get(23)?,
                                row.get(24)?,
                                row.get(25)?,
                                row.get(26)?,
                                row.get(27)?,
                            ),
                            validation_status: row.get(28)?,
                            validated_at_ms: row.get(29)?,
                            validation_message: row.get(30)?,
                        },
                    })
                })
                .map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    pub fn profile(&self, id: &str) -> AppResult<StoredProfile> {
        self.list_profiles()?
            .into_iter()
            .find(|stored| stored.profile.id == id)
            .ok_or(AppError::NotFound)
    }

    pub fn update_profile(&self, stored: &StoredProfile) -> AppResult<()> {
        self.with_connection(|connection| {
            let updated = connection
                .execute(
                    "UPDATE profiles SET alias = ?2, provider = ?3, wire_api = ?4, enabled = ?5, in_pool = ?6, priority = ?7, weight = ?8, max_concurrency = ?9, max_queue_depth = ?10, queue_timeout_ms = ?11, models_json = ?12, model_mappings_json = ?13, codex_oauth_profile_id = ?14, secret_ref = ?15, credential_configured = ?16, auth_mode = ?17, credential_fingerprint = ?18, account_display_name = ?19, account_email = ?20, account_id = ?21, account_updated_at_ms = ?22, account_quota_json = ?23, account_subscription_json = ?24, health = ?25, cooldown_until_ms = ?26, validation_status = ?27, validated_at_ms = ?28, validation_message = ?29 WHERE id = ?1",
                    params![
                        stored.profile.id,
                        stored.profile.alias,
                        provider_name(&stored.profile.provider),
                        wire_api_name(&stored.profile.wire_api),
                        stored.profile.enabled,
                        stored.profile.in_pool,
                        stored.profile.priority,
                        stored.profile.weight,
                        stored.profile.max_concurrency,
                        stored.profile.max_queue_depth,
                        stored.profile.queue_timeout_ms,
                        serde_json::to_string(&stored.profile.models).map_err(|_| AppError::Internal)?,
                        serde_json::to_string(&stored.profile.model_mappings).map_err(|_| AppError::Internal)?,
                        stored.profile.codex_oauth_profile_id,
                        stored.secret_ref,
                        stored.profile.credential_configured,
                        auth_mode_name(&stored.profile.auth_mode),
                        stored.credential_fingerprint,
                        stored.profile.account.as_ref().and_then(|account| account.display_name.clone()),
                        stored.profile.account.as_ref().and_then(|account| account.email.clone()),
                        stored.profile.account.as_ref().and_then(|account| account.account_id.clone()),
                        stored.profile.account.as_ref().map(|account| account.updated_at_ms),
                        stored.profile.account.as_ref().and_then(|account| serde_json::to_string(&account.quota).ok()),
                        stored.profile.account.as_ref().and_then(|account| serde_json::to_string(&account.subscription).ok()),
                        stored.profile.health,
                        stored.profile.cooldown_until_ms,
                        stored.profile.validation_status,
                        stored.profile.validated_at_ms,
                        stored.profile.validation_message,
                    ],
                )
                .map_err(|_| AppError::Internal)?;
            if updated == 0 { return Err(AppError::NotFound); }
            Ok(())
        })
    }

    pub fn delete_profile(&self, id: &str) -> AppResult<Option<String>> {
        let stored = self.profile(id)?;
        self.with_connection(|connection| {
            connection
                .execute("DELETE FROM profiles WHERE id = ?1", params![id])
                .map_err(|_| AppError::Internal)?;
            connection
                .execute(
                    "DELETE FROM app_settings WHERE key = 'current_profile' AND value = ?1",
                    params![id],
                )
                .map_err(|_| AppError::Internal)?;
            connection
                .execute(
                    "UPDATE profiles SET codex_oauth_profile_id = NULL WHERE codex_oauth_profile_id = ?1",
                    params![id],
                )
                .map_err(|_| AppError::Internal)?;
            for setting_key in [
                GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
                GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING,
            ] {
                connection
                    .execute(
                        "DELETE FROM app_settings WHERE key = ?1 AND value = ?2",
                        params![setting_key, id],
                    )
                    .map_err(|_| AppError::Internal)?;
            }
            Ok(stored.secret_ref)
        })
    }

    pub fn set_current_profile(&self, id: &str) -> AppResult<()> {
        let _ = self.profile(id)?;
        self.with_connection(|connection| {
            connection.execute("INSERT INTO app_settings(key, value) VALUES('current_profile', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![id]).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn setting(&self, key: &str) -> AppResult<Option<String>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO app_settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn delete_setting(&self, key: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute("DELETE FROM app_settings WHERE key = ?1", params![key])
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn ensure_local_secret_store_backend(&self) -> AppResult<bool> {
        const LOCAL_BACKEND: &str = "local_vault_v1";
        if self.setting("secret_store_backend")?.as_deref() == Some(LOCAL_BACKEND) {
            return Ok(false);
        }
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE profiles SET credential_configured = 0, health = 'reauthorization_required' WHERE credential_configured = 1",
                    [],
                )
                .map_err(|_| AppError::Internal)?;
            connection
                .execute(
                    "INSERT INTO app_settings(key, value) VALUES('secret_store_backend', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![LOCAL_BACKEND],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(true)
        })
    }

    pub fn desktop_workspace_mode(&self) -> AppResult<DesktopWorkspaceMode> {
        match self.setting("desktop_workspace_mode")?.as_deref() {
            None | Some("fresh") | Some("per_profile") | Some("shared") => {
                Ok(DesktopWorkspaceMode::Shared)
            }
            Some(_) => Err(AppError::ValidationFailed),
        }
    }

    pub fn set_desktop_workspace_mode(&self, _mode: &DesktopWorkspaceMode) -> AppResult<()> {
        self.set_setting("desktop_workspace_mode", "shared")
    }

    pub fn app_update_settings(&self) -> AppResult<AppUpdateSettings> {
        let channel = match self.setting("app_update_channel")?.as_deref() {
            None | Some("stable") => AppUpdateChannel::Stable,
            Some("beta") => AppUpdateChannel::Beta,
            Some(_) => return Err(AppError::ValidationFailed),
        };
        let auto_check = match self.setting("app_update_auto_check")?.as_deref() {
            None | Some("true") => true,
            Some("false") => false,
            Some(_) => return Err(AppError::ValidationFailed),
        };
        Ok(AppUpdateSettings {
            channel,
            auto_check,
        })
    }

    pub fn set_app_update_settings(
        &self,
        channel: AppUpdateChannel,
        auto_check: bool,
    ) -> AppResult<AppUpdateSettings> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO app_settings(key, value) VALUES('app_update_channel', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![channel.as_setting_value()],
                )
                .map_err(|_| AppError::Internal)?;
            connection
                .execute(
                    "INSERT INTO app_settings(key, value) VALUES('app_update_auto_check', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![if auto_check { "true" } else { "false" }],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })?;
        Ok(AppUpdateSettings {
            channel,
            auto_check,
        })
    }

    #[cfg(test)]
    pub fn create_desktop_workspace(
        &self,
        id: &str,
        profile_id: &str,
        timestamp_ms: i64,
    ) -> AppResult<()> {
        let _ = self.profile(profile_id)?;
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO desktop_workspaces(id, profile_id, created_at_ms, last_launched_at_ms) VALUES(?1, ?2, ?3, ?3)",
                    params![id, profile_id, timestamp_ms],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn desktop_workspace(&self, id: &str) -> AppResult<StoredDesktopWorkspace> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT id, profile_id FROM desktop_workspaces WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(StoredDesktopWorkspace {
                            id: row.get(0)?,
                            profile_id: row.get(1)?,
                        })
                    },
                )
                .optional()
                .map_err(|_| AppError::Internal)?
                .ok_or(AppError::NotFound)
        })
    }

    pub fn list_desktop_workspaces(&self) -> AppResult<Vec<DesktopWorkspaceHistoryItem>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT w.id, w.profile_id, COALESCE(p.alias, '已删除档案'), w.created_at_ms, w.last_launched_at_ms
                     FROM desktop_workspaces w LEFT JOIN profiles p ON p.id = w.profile_id
                     ORDER BY w.last_launched_at_ms DESC, w.created_at_ms DESC",
                )
                .map_err(|_| AppError::Internal)?;
            let workspaces = statement
                .query_map([], |row| {
                    Ok(DesktopWorkspaceHistoryItem {
                        id: row.get(0)?,
                        profile_id: row.get(1)?,
                        profile_alias: row.get(2)?,
                        created_at_ms: row.get(3)?,
                        last_launched_at_ms: row.get(4)?,
                    })
                })
                .map_err(|_| AppError::Internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AppError::Internal)?;
            Ok(workspaces)
        })
    }

    pub fn touch_desktop_workspace(&self, id: &str, timestamp_ms: i64) -> AppResult<()> {
        self.with_connection(|connection| {
            let updated = connection
                .execute(
                    "UPDATE desktop_workspaces SET last_launched_at_ms = ?2 WHERE id = ?1",
                    params![id, timestamp_ms],
                )
                .map_err(|_| AppError::Internal)?;
            (updated == 1).then_some(()).ok_or(AppError::NotFound)
        })
    }

    pub fn delete_desktop_workspace(&self, id: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            let deleted = connection
                .execute("DELETE FROM desktop_workspaces WHERE id = ?1", params![id])
                .map_err(|_| AppError::Internal)?;
            (deleted == 1).then_some(()).ok_or(AppError::NotFound)
        })
    }

    pub fn gateway_settings(
        &self,
        running: bool,
        certificate_ready: bool,
        available_addresses: Vec<GatewayNetworkAddress>,
    ) -> AppResult<GatewayStatus> {
        let profiles = self.list_profiles()?;
        let now_ms = timestamp_ms();
        self.with_connection(|connection| {
            let (bind_mode, bind_address, port, cidrs_json): (String, String, u16, String) = connection
                .query_row("SELECT bind_mode, bind_address, port, cidrs_json FROM gateway_settings WHERE id = 1", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .map_err(|_| AppError::Internal)?;
            let client_key_count = connection.query_row("SELECT COUNT(*) FROM client_keys WHERE revoked = 0", [], |row| row.get::<_, i64>(0)).map_err(|_| AppError::Internal)? as usize;
            let upstream_proxy_mode = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = 'gateway_upstream_proxy_mode'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?
                .unwrap_or_else(|| "system".to_owned());
            let upstream_proxy_display = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = 'gateway_upstream_proxy_display'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let upstream_last_error = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = 'gateway_upstream_last_error'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let available_profiles = profiles.iter().filter(|p| {
                let profile = &p.profile;
                (profile.kind == ProfileKind::ApiKey
                    || (profile.kind == ProfileKind::CodexOauth
                        && profile.auth_mode == CodexAuthMode::OAuth))
                    && profile.enabled
                    && profile.in_pool
                    && profile.credential_configured
                    && !matches!(profile.health.as_str(), "unhealthy" | "reauthorization_required")
                    && profile.validation_status != "invalid"
                    && profile.cooldown_until_ms.is_none_or(|until| until <= now_ms)
            }).count();
            let direct_profile_id = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = ?1",
                    params![GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let direct_route = direct_profile_id.and_then(|profile_id| {
                profiles.iter().find(|stored| stored.profile.id == profile_id).map(|stored| {
                    let profile = &stored.profile;
                    let credential_ready = profile.enabled
                        && profile.credential_configured
                        && stored.secret_ref.is_some()
                        && profile.validation_status != "invalid"
                        && !matches!(profile.health.as_str(), "unhealthy" | "reauthorization_required");
                    let oauth_ready = profile.codex_oauth_profile_id.as_ref().is_some_and(|oauth_id| {
                        profiles.iter().any(|oauth| {
                            oauth.profile.id == *oauth_id
                                && oauth.profile.kind == ProfileKind::CodexOauth
                                && oauth.profile.auth_mode == CodexAuthMode::OAuth
                                && oauth.credential_fingerprint.is_none()
                                && oauth.profile.enabled
                                && oauth.profile.credential_configured
                                && oauth.profile.validation_status != "invalid"
                                && !matches!(oauth.profile.health.as_str(), "unhealthy" | "reauthorization_required")
                        })
                    });
                    GatewayDirectRouteHealth {
                        status: if credential_ready && oauth_ready { "ok" } else if credential_ready { "degraded" } else { "unavailable" }.to_owned(),
                        profile_id: profile.id.clone(),
                        profile_alias: profile.alias.clone(),
                        route_mode: if oauth_ready { "relay_bridge" } else { "provider_direct" }.to_owned(),
                        oauth_ready,
                        credential_ready,
                        model_count: profile.models.len(),
                    }
                })
            });
            Ok(GatewayStatus {
                running,
                bind_mode,
                bind_address: bind_address.clone(),
                available_addresses,
                port,
                cidrs: serde_json::from_str(&cidrs_json).unwrap_or_default(),
                available_profiles,
                cooling_profiles: profiles.iter().filter(|p| {
                    let profile = &p.profile;
                    (profile.kind == ProfileKind::ApiKey
                        || (profile.kind == ProfileKind::CodexOauth
                            && profile.auth_mode == CodexAuthMode::OAuth))
                        && profile.cooldown_until_ms.is_some()
                }).count(),
                pool_status: if available_profiles > 0 { "ok" } else { "unavailable" }.to_owned(),
                direct_route,
                active_requests: 0,
                queued_requests: 0,
                client_key_count,
                certificate_ready,
                service_url: format!("https://{}:{}", bind_address, port),
                upstream_proxy_mode,
                upstream_proxy_display,
                upstream_last_error,
            })
        })
    }

    pub fn update_gateway_settings(
        &self,
        bind_mode: &str,
        bind_address: &str,
        port: u16,
        cidrs: &[String],
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute("UPDATE gateway_settings SET bind_mode = ?1, bind_address = ?2, port = ?3, cidrs_json = ?4 WHERE id = 1", params![bind_mode, bind_address, port, serde_json::to_string(cidrs).map_err(|_| AppError::Internal)?]).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn insert_client_key(
        &self,
        key: &MaskedClientKey,
        hash: &str,
        secret_ref: &str,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute("INSERT INTO client_keys(id, name, key_hash, masked_value, created_at_ms, last_used_at_ms, revoked, secret_ref) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![key.id, key.name, hash, key.masked_value, key.created_at_ms, key.last_used_at_ms, key.revoked, secret_ref]).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn list_client_keys(&self) -> AppResult<Vec<MaskedClientKey>> {
        self.with_connection(|connection| {
            let codex_key_ref = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = ?1",
                    params![GATEWAY_CODEX_CLIENT_KEY_REF_SETTING],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let mut statement = connection.prepare("SELECT id, name, masked_value, created_at_ms, last_used_at_ms, revoked, secret_ref FROM client_keys WHERE revoked = 0 ORDER BY created_at_ms DESC").map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], |row| {
                let secret_ref: String = row.get(6)?;
                let managed_by = if codex_key_ref.as_deref() == Some(secret_ref.as_str()) {
                    "codex_gateway"
                } else {
                    "user"
                }
                .to_owned();
                Ok(MaskedClientKey {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    masked_value: row.get(2)?,
                    created_at_ms: row.get(3)?,
                    last_used_at_ms: row.get(4)?,
                    revoked: row.get(5)?,
                    can_revoke: managed_by == "user",
                    managed_by,
                })
            }).map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    pub fn user_client_key_secret_ref(&self, id: &str) -> AppResult<String> {
        self.with_connection(|connection| {
            let secret_ref = connection
                .query_row(
                    "SELECT secret_ref FROM client_keys WHERE id = ?1 AND revoked = 0",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?
                .ok_or(AppError::NotFound)?;
            let codex_key_ref = connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = ?1",
                    params![GATEWAY_CODEX_CLIENT_KEY_REF_SETTING],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            if codex_key_ref.as_deref() == Some(secret_ref.as_str()) {
                return Err(AppError::Conflict);
            }
            Ok(secret_ref)
        })
    }

    pub fn update_client_key_material(
        &self,
        id: &str,
        hash: &str,
        masked_value: &str,
    ) -> AppResult<String> {
        self.with_connection(|connection| {
            let secret_ref = user_client_key_secret_ref_from_connection(connection, id)?;
            let updated = connection
                .execute(
                    "UPDATE client_keys SET key_hash = ?2, masked_value = ?3, last_used_at_ms = NULL WHERE id = ?1 AND revoked = 0",
                    params![id, hash, masked_value],
                )
                .map_err(|_| AppError::Internal)?;
            (updated == 1).then_some(secret_ref).ok_or(AppError::NotFound)
        })
    }

    pub fn revoke_client_key(&self, id: &str) -> AppResult<Option<String>> {
        self.with_connection(|connection| {
            let secret_ref = user_client_key_secret_ref_from_connection(connection, id)?;
            connection
                .execute(
                    "UPDATE client_keys SET revoked = 1 WHERE id = ?1",
                    params![id],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(Some(secret_ref))
        })
    }

    pub fn valid_key_hashes(&self) -> AppResult<Vec<(String, String, String)>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT id, key_hash, secret_ref FROM client_keys WHERE revoked = 0")
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| AppError::Internal)
        })
    }

    pub fn record_key_use(&self, id: &str, timestamp_ms: i64) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE client_keys SET last_used_at_ms = ?2 WHERE id = ?1",
                    params![id, timestamp_ms],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn legacy_channel_secret_refs(&self) -> AppResult<Vec<String>> {
        self.with_connection(|connection| {
            let exists: Option<String> = connection
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'channels'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            if exists.is_none() {
                return Ok(Vec::new());
            }

            let mut statement = connection
                .prepare("SELECT endpoint_ref, signing_ref FROM channels")
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], |row| {
                    let endpoint_ref: String = row.get(0)?;
                    let signing_ref: Option<String> = row.get(1)?;
                    Ok((endpoint_ref, signing_ref))
                })
                .map_err(|_| AppError::Internal)?;
            let mut references = Vec::new();
            for row in rows {
                let (endpoint_ref, signing_ref) = row.map_err(|_| AppError::Internal)?;
                references.push(endpoint_ref);
                if let Some(signing_ref) = signing_ref {
                    references.push(signing_ref);
                }
            }
            Ok(references)
        })
    }

    pub fn drop_legacy_channels(&self) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute("DROP TABLE IF EXISTS channels", [])
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn upsert_collaboration_bot(&self, stored: &StoredCollaborationBot) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO collaboration_bots(id, provider, name, enabled, credential_mask, config_summary, config_json, secret_refs_json, connection_status, last_error, callback_public_url, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(id) DO UPDATE SET provider = excluded.provider, name = excluded.name, enabled = excluded.enabled, credential_mask = excluded.credential_mask, config_summary = excluded.config_summary, config_json = excluded.config_json, secret_refs_json = excluded.secret_refs_json, connection_status = excluded.connection_status, last_error = excluded.last_error, callback_public_url = excluded.callback_public_url, updated_at_ms = excluded.updated_at_ms",
                params![
                    stored.bot.id,
                    collaboration_provider_name(&stored.bot.provider),
                    stored.bot.name,
                    stored.bot.enabled,
                    stored.bot.credential_mask,
                    stored.bot.config_summary,
                    stored.config_json,
                    stored.secret_refs_json,
                    stored.bot.connection_status,
                    stored.bot.last_error,
                    stored.bot.callback_public_url,
                    stored.bot.updated_at_ms,
                ],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn list_collaboration_bots(&self) -> AppResult<Vec<StoredCollaborationBot>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, provider, name, enabled, credential_mask, config_summary, config_json, secret_refs_json, connection_status, last_error, callback_public_url, updated_at_ms FROM collaboration_bots ORDER BY provider, name",
            ).map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], |row| {
                let provider: String = row.get(1)?;
                let config_json: String = row.get(6)?;
                Ok(StoredCollaborationBot {
                    bot: MaskedCollaborationBot {
                        id: row.get(0)?,
                        provider: parse_collaboration_provider(&provider)
                            .unwrap_or(CollaborationProvider::Feishu),
                        name: row.get(2)?,
                        enabled: row.get(3)?,
                        credential_mask: row.get(4)?,
                        config_summary: row.get(5)?,
                        connection_status: row.get(8)?,
                        last_error: row.get(9)?,
                        callback_public_url: row.get(10)?,
                        system_prompt: system_prompt_from_config_json(&config_json),
                        updated_at_ms: row.get(11)?,
                    },
                    config_json,
                    secret_refs_json: row.get(7)?,
                })
            }).map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    pub fn collaboration_summary(&self) -> AppResult<CollaborationSummary> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM collaboration_bots WHERE enabled = 1),
                       (SELECT COUNT(*) FROM collaboration_project_bindings WHERE chat_id IS NOT NULL AND chat_id != ''),
                       (SELECT COUNT(*) FROM codex_sessions WHERE relay_status = 'running')",
                    [],
                    |row| {
                        Ok(CollaborationSummary {
                            enabled_bots: row.get(0)?,
                            bound_chats: row.get(1)?,
                            active_sessions: row.get(2)?,
                        })
                    },
                )
                .map_err(|_| AppError::Internal)
        })
    }

    pub fn collaboration_bot(&self, id: &str) -> AppResult<StoredCollaborationBot> {
        self.list_collaboration_bots()?
            .into_iter()
            .find(|item| item.bot.id == id)
            .ok_or(AppError::NotFound)
    }

    pub fn delete_collaboration_bot(&self, id: &str) -> AppResult<Vec<String>> {
        let stored = self.collaboration_bot(id)?;
        let refs = serde_json::from_str::<std::collections::HashMap<String, String>>(
            &stored.secret_refs_json,
        )
        .map(|map| map.into_values().collect::<Vec<_>>())
        .unwrap_or_default();
        self.with_connection(|connection| {
            connection
                .execute("DELETE FROM collaboration_bots WHERE id = ?1", params![id])
                .map_err(|_| AppError::Internal)?;
            Ok(refs)
        })
    }

    pub fn update_collaboration_bot_status(
        &self,
        id: &str,
        status: &str,
        last_error: Option<&str>,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE collaboration_bots SET connection_status = ?2, last_error = ?3, updated_at_ms = ?4 WHERE id = ?1",
                params![id, status, last_error, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn upsert_collaboration_project_binding(
        &self,
        binding: &CollaborationProjectBinding,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO collaboration_project_bindings(id, provider, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, execution_target, model_id, created_at_ms, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(id) DO UPDATE SET provider = excluded.provider, bot_id = excluded.bot_id, project_name = excluded.project_name, project_slug = excluded.project_slug, working_directory = excluded.working_directory, profile_id = excluded.profile_id, enabled = excluded.enabled, concurrency_limit = excluded.concurrency_limit, execution_target = excluded.execution_target, model_id = excluded.model_id, updated_at_ms = excluded.updated_at_ms",
                params![
                    binding.id,
                    collaboration_provider_name(&binding.provider),
                    binding.bot_id,
                    binding.project_name,
                    binding.project_slug,
                    binding.working_directory,
                    binding.profile_id.as_deref(),
                    binding.chat_id,
                    binding.bind_code,
                    binding.enabled,
                    binding.concurrency_limit,
                    binding.execution_target,
                    binding.model_id,
                    binding.created_at_ms,
                    binding.updated_at_ms,
                ],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn list_collaboration_project_bindings(
        &self,
    ) -> AppResult<Vec<CollaborationProjectBinding>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT b.id, b.provider, b.bot_id, bot.name, b.project_name, b.project_slug, b.working_directory, b.profile_id, p.alias, b.chat_id, b.bind_code, b.enabled, b.concurrency_limit, b.execution_target, b.model_id, b.created_at_ms, b.updated_at_ms
                 FROM collaboration_project_bindings b
                 JOIN collaboration_bots bot ON bot.id = b.bot_id
                 LEFT JOIN profiles p ON p.id = b.profile_id
                 ORDER BY b.provider, b.project_name",
            ).map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], collaboration_binding_from_row).map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    pub fn collaboration_project_binding(
        &self,
        id: &str,
    ) -> AppResult<CollaborationProjectBinding> {
        self.list_collaboration_project_bindings()?
            .into_iter()
            .find(|binding| binding.id == id)
            .ok_or(AppError::NotFound)
    }

    pub fn collaboration_binding_by_code(
        &self,
        code: &str,
    ) -> AppResult<CollaborationProjectBinding> {
        self.list_collaboration_project_bindings()?
            .into_iter()
            .find(|binding| binding.bind_code == code)
            .ok_or(AppError::NotFound)
    }

    pub fn collaboration_bindings_for_chat(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
    ) -> AppResult<Vec<CollaborationProjectBinding>> {
        Ok(self
            .list_collaboration_project_bindings()?
            .into_iter()
            .filter(|binding| {
                binding.provider == provider
                    && binding.bot_id == bot_id
                    && binding.chat_id.as_deref() == Some(chat_id)
                    && binding.enabled
            })
            .collect())
    }

    pub fn bind_collaboration_project_chat(
        &self,
        id: &str,
        chat_id: &str,
    ) -> AppResult<CollaborationProjectBinding> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_project_bindings SET chat_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![id, chat_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 {
                return Err(AppError::NotFound);
            }
            Ok(())
        })?;
        self.collaboration_project_binding(id)
    }

    pub fn delete_collaboration_project_binding(&self, id: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "DELETE FROM collaboration_project_bindings WHERE id = ?1",
                    params![id],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn set_provider_state(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        key: &str,
        value: &str,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO collaboration_provider_state(provider, bot_id, state_key, state_value, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(provider, bot_id, state_key) DO UPDATE SET state_value = excluded.state_value, updated_at_ms = excluded.updated_at_ms",
                params![
                    collaboration_provider_name(&provider),
                    bot_id,
                    key,
                    value,
                    timestamp_ms(),
                ],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn provider_state(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        key: &str,
    ) -> AppResult<Option<String>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT state_value FROM collaboration_provider_state WHERE provider = ?1 AND bot_id = ?2 AND state_key = ?3",
                    params![collaboration_provider_name(&provider), bot_id, key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)
        })
    }

    pub fn upsert_collaboration_context(
        &self,
        context: &CollaborationContextSummary,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO collaboration_contexts(id, scope_key, binding_id, memory_enabled, permissions_policy, model_id, active_codex_session_id, active_relay_session_id, goal_status, goal_text, conversation_mode, last_turn_at_ms, created_at_ms, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(id) DO UPDATE SET scope_key = excluded.scope_key, binding_id = excluded.binding_id, memory_enabled = excluded.memory_enabled, permissions_policy = excluded.permissions_policy, model_id = excluded.model_id, active_codex_session_id = excluded.active_codex_session_id, active_relay_session_id = excluded.active_relay_session_id, goal_status = excluded.goal_status, goal_text = excluded.goal_text, conversation_mode = excluded.conversation_mode, last_turn_at_ms = excluded.last_turn_at_ms, updated_at_ms = excluded.updated_at_ms",
                params![
                    context.id,
                    context.scope_key,
                    context.binding_id,
                    context.memory_enabled,
                    context.permissions_policy,
                    context.model_id,
                    context.active_codex_session_id,
                    context.active_relay_session_id,
                    context.goal_status,
                    context.goal_text,
                    context.conversation_mode,
                    context.last_turn_at_ms,
                    context.created_at_ms,
                    context.updated_at_ms,
                ],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn list_collaboration_contexts(&self) -> AppResult<Vec<CollaborationContextSummary>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(COLLABORATION_CONTEXT_SELECT)
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], collaboration_context_from_row)
                .map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| AppError::Internal)
        })
    }

    pub fn collaboration_context(&self, id: &str) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    &format!("{COLLABORATION_CONTEXT_SELECT} WHERE c.id = ?1"),
                    params![id],
                    collaboration_context_from_row,
                )
                .map_err(map_session_lookup_error)
        })
    }

    pub fn collaboration_context_by_scope(
        &self,
        scope_key: &str,
    ) -> AppResult<Option<CollaborationContextSummary>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    &format!("{COLLABORATION_CONTEXT_SELECT} WHERE c.scope_key = ?1"),
                    params![scope_key],
                    collaboration_context_from_row,
                )
                .optional()
                .map_err(|_| AppError::Internal)
        })
    }

    pub fn set_collaboration_chat_context(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
        context_id: &str,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO collaboration_chat_state(provider, bot_id, chat_id, context_id, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(provider, bot_id, chat_id) DO UPDATE SET context_id = excluded.context_id, updated_at_ms = excluded.updated_at_ms",
                params![collaboration_provider_name(&provider), bot_id, chat_id, context_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn collaboration_chat_context(
        &self,
        provider: CollaborationProvider,
        bot_id: &str,
        chat_id: &str,
    ) -> AppResult<Option<CollaborationContextSummary>> {
        self.with_connection(|connection| {
            connection.query_row(
                &format!(
                    "{COLLABORATION_CONTEXT_SELECT} JOIN collaboration_chat_state cs ON cs.context_id = c.id WHERE cs.provider = ?1 AND cs.bot_id = ?2 AND cs.chat_id = ?3"
                ),
                params![collaboration_provider_name(&provider), bot_id, chat_id],
                collaboration_context_from_row,
            ).optional().map_err(|_| AppError::Internal)
        })
    }

    pub fn set_collaboration_context_memory(
        &self,
        context_id: &str,
        enabled: bool,
    ) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET memory_enabled = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, enabled, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })?;
        self.collaboration_context(context_id)
    }

    pub fn set_collaboration_context_goal(
        &self,
        context_id: &str,
        status: &str,
        text: Option<&str>,
    ) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET goal_status = ?2, goal_text = ?3, updated_at_ms = ?4 WHERE id = ?1",
                params![context_id, status, text, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })?;
        self.collaboration_context(context_id)
    }

    pub fn set_collaboration_context_mode(
        &self,
        context_id: &str,
        mode: &str,
    ) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET conversation_mode = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, mode, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })?;
        self.collaboration_context(context_id)
    }

    pub fn set_collaboration_context_permissions(
        &self,
        context_id: &str,
        policy: &str,
    ) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET permissions_policy = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, policy, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })?;
        self.collaboration_context(context_id)
    }

    pub fn set_collaboration_context_model(
        &self,
        context_id: &str,
        model_id: Option<&str>,
    ) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET model_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, model_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            connection.execute(
                "UPDATE collaboration_project_bindings SET model_id = ?2, updated_at_ms = ?3 WHERE id = (SELECT binding_id FROM collaboration_contexts WHERE id = ?1)",
                params![context_id, model_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })?;
        self.collaboration_context(context_id)
    }

    pub fn clear_collaboration_context_active(
        &self,
        context_id: &str,
    ) -> AppResult<CollaborationContextSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET active_codex_session_id = NULL, active_relay_session_id = NULL, conversation_mode = 'default', updated_at_ms = ?2 WHERE id = ?1",
                params![context_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })?;
        self.collaboration_context(context_id)
    }

    pub fn mark_collaboration_context_turn_started(
        &self,
        context_id: &str,
        relay_session_id: &str,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET active_relay_session_id = ?2, last_turn_at_ms = ?3, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, relay_session_id, timestamp_ms()],
            ).map_err(map_local_state_error)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })
    }

    pub fn set_collaboration_context_codex_id(
        &self,
        context_id: &str,
        codex_session_id: &str,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE collaboration_contexts SET active_codex_session_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, codex_session_id, timestamp_ms()],
            ).map_err(map_local_state_error)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })
    }

    pub fn mark_collaboration_context_turn_finished(
        &self,
        context_id: &str,
        relay_session_id: &str,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE collaboration_contexts SET active_relay_session_id = CASE WHEN active_relay_session_id = ?2 THEN NULL ELSE active_relay_session_id END, last_turn_at_ms = ?3, updated_at_ms = ?3 WHERE id = ?1",
                params![context_id, relay_session_id, timestamp_ms()],
            ).map_err(map_local_state_error)?;
            Ok(())
        })
    }

    pub fn count_running_codex_sessions_for_context(&self, context_id: &str) -> AppResult<i64> {
        self.with_connection(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM codex_sessions WHERE context_id = ?1 AND relay_status = 'running'",
                params![context_id],
                |row| row.get(0),
            ).map_err(map_local_state_error)
        })
    }

    #[allow(dead_code)]
    pub fn insert_feishu_bot(&self, stored: &StoredFeishuBot) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO feishu_bots(id, name, app_id, app_id_mask, app_secret_ref, enabled, connection_status, last_error, updated_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    stored.bot.id,
                    stored.bot.name,
                    stored.bot.app_id,
                    stored.bot.app_id_mask,
                    stored.app_secret_ref,
                    stored.bot.enabled,
                    stored.bot.connection_status,
                    stored.bot.last_error,
                    stored.bot.updated_at_ms,
                ],
            ).map_err(map_local_state_error)?;
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn update_feishu_bot(&self, stored: &StoredFeishuBot) -> AppResult<()> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE feishu_bots SET name = ?2, app_id = ?3, app_id_mask = ?4, app_secret_ref = ?5, enabled = ?6, connection_status = ?7, last_error = ?8, updated_at_ms = ?9 WHERE id = ?1",
                params![
                    stored.bot.id,
                    stored.bot.name,
                    stored.bot.app_id,
                    stored.bot.app_id_mask,
                    stored.app_secret_ref,
                    stored.bot.enabled,
                    stored.bot.connection_status,
                    stored.bot.last_error,
                    stored.bot.updated_at_ms,
                ],
            ).map_err(map_local_state_error)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn list_feishu_bots(&self) -> AppResult<Vec<StoredFeishuBot>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare("SELECT id, name, app_id, app_id_mask, app_secret_ref, enabled, connection_status, last_error, updated_at_ms FROM feishu_bots ORDER BY name").map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], |row| {
                Ok(StoredFeishuBot {
                    bot: MaskedFeishuBot {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        app_id: row.get(2)?,
                        app_id_mask: row.get(3)?,
                        enabled: row.get(5)?,
                        connection_status: row.get(6)?,
                        last_error: row.get(7)?,
                        updated_at_ms: row.get(8)?,
                    },
                    app_secret_ref: row.get(4)?,
                })
            }).map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    #[allow(dead_code)]
    pub fn feishu_bot(&self, id: &str) -> AppResult<StoredFeishuBot> {
        self.list_feishu_bots()?
            .into_iter()
            .find(|item| item.bot.id == id)
            .ok_or(AppError::NotFound)
    }

    #[allow(dead_code)]
    pub fn delete_feishu_bot(&self, id: &str) -> AppResult<String> {
        let stored = self.feishu_bot(id)?;
        self.with_connection(|connection| {
            connection
                .execute("DELETE FROM feishu_bots WHERE id = ?1", params![id])
                .map_err(|_| AppError::Internal)?;
            Ok(stored.app_secret_ref)
        })
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub fn update_feishu_bot_status(
        &self,
        id: &str,
        status: &str,
        last_error: Option<&str>,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE feishu_bots SET connection_status = ?2, last_error = ?3, updated_at_ms = ?4 WHERE id = ?1",
                params![id, status, last_error, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn upsert_feishu_project_binding(&self, binding: &FeishuProjectBinding) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO feishu_project_bindings(id, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, created_at_ms, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(id) DO UPDATE SET bot_id = excluded.bot_id, project_name = excluded.project_name, project_slug = excluded.project_slug, working_directory = excluded.working_directory, profile_id = excluded.profile_id, enabled = excluded.enabled, concurrency_limit = excluded.concurrency_limit, updated_at_ms = excluded.updated_at_ms",
                params![
                    binding.id,
                    binding.bot_id,
                    binding.project_name,
                    binding.project_slug,
                    binding.working_directory,
                    binding.profile_id,
                    binding.chat_id,
                    binding.bind_code,
                    binding.enabled,
                    binding.concurrency_limit,
                    binding.created_at_ms,
                    binding.updated_at_ms,
                ],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn list_feishu_project_bindings(&self) -> AppResult<Vec<FeishuProjectBinding>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT b.id, b.bot_id, bot.name, b.project_name, b.project_slug, b.working_directory, b.profile_id, p.alias, b.chat_id, b.bind_code, b.enabled, b.concurrency_limit, b.created_at_ms, b.updated_at_ms
                 FROM feishu_project_bindings b
                 JOIN feishu_bots bot ON bot.id = b.bot_id
                 JOIN profiles p ON p.id = b.profile_id
                 ORDER BY b.project_name",
            ).map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], feishu_binding_from_row).map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    #[allow(dead_code)]
    pub fn feishu_project_binding(&self, id: &str) -> AppResult<FeishuProjectBinding> {
        self.list_feishu_project_bindings()?
            .into_iter()
            .find(|binding| binding.id == id)
            .ok_or(AppError::NotFound)
    }

    #[allow(dead_code)]
    pub fn feishu_binding_by_code(&self, code: &str) -> AppResult<FeishuProjectBinding> {
        self.list_feishu_project_bindings()?
            .into_iter()
            .find(|binding| binding.bind_code == code)
            .ok_or(AppError::NotFound)
    }

    #[allow(dead_code)]
    pub fn feishu_bindings_for_chat(
        &self,
        bot_id: &str,
        chat_id: &str,
    ) -> AppResult<Vec<FeishuProjectBinding>> {
        Ok(self
            .list_feishu_project_bindings()?
            .into_iter()
            .filter(|binding| {
                binding.bot_id == bot_id
                    && binding.chat_id.as_deref() == Some(chat_id)
                    && binding.enabled
            })
            .collect())
    }

    #[allow(dead_code)]
    pub fn bind_feishu_project_chat(
        &self,
        id: &str,
        chat_id: &str,
    ) -> AppResult<FeishuProjectBinding> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE feishu_project_bindings SET chat_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![id, chat_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
            if changed == 0 {
                return Err(AppError::NotFound);
            }
            Ok(())
        })?;
        self.feishu_project_binding(id)
    }

    #[allow(dead_code)]
    pub fn delete_feishu_project_binding(&self, id: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "DELETE FROM feishu_project_bindings WHERE id = ?1",
                    params![id],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn insert_codex_session(&self, stored: &StoredCodexSession) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute("PRAGMA foreign_keys = OFF", [])
                .map_err(map_local_state_error)?;
            connection.execute(
                "INSERT INTO codex_sessions(id, binding_id, context_id, profile_id, provider, provider_bot_id, provider_chat_id, provider_message_id, relay_status, codex_session_id, feishu_message_id, feishu_chat_id, started_by, started_at_ms, updated_at_ms, finished_at_ms, summary, last_error, execution_target, model_id, turn_kind, conversation_mode, goal_status, working_directory)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
                params![
                    stored.session.id,
                    stored.session.binding_id,
                    stored.session.context_id.as_deref(),
                    stored.session.profile_id.as_deref(),
                    collaboration_provider_name(&stored.session.provider),
                    stored.session.provider_bot_id,
                    stored.session.provider_chat_id,
                    stored.session.provider_message_id,
                    stored.session.relay_status,
                    stored.session.codex_session_id,
                    stored.session.feishu_message_id,
                    stored.session.feishu_chat_id,
                    stored.session.started_by,
                    stored.session.started_at_ms,
                    stored.session.updated_at_ms,
                    stored.session.finished_at_ms,
                    stored.session.summary,
                    stored.session.last_error,
                    stored.session.execution_target,
                    stored.session.model_id,
                    stored.session.turn_kind,
                    stored.session.conversation_mode,
                    stored.session.goal_status,
                    stored.working_directory,
                ],
            ).map_err(map_local_state_error)?;
            connection
                .execute("PRAGMA foreign_keys = ON", [])
                .map_err(map_local_state_error)?;
            Ok(())
        })
    }

    pub fn set_codex_session_status(
        &self,
        id: &str,
        status: &str,
        summary: Option<&str>,
        last_error: Option<&str>,
        finished_at_ms: Option<i64>,
    ) -> AppResult<CodexSessionSummary> {
        self.with_connection(|connection| {
            let changed = connection.execute(
                "UPDATE codex_sessions SET relay_status = ?2, summary = COALESCE(?3, summary), last_error = ?4, finished_at_ms = ?5, updated_at_ms = ?6 WHERE id = ?1",
                params![id, status, summary, last_error, finished_at_ms, timestamp_ms()],
            ).map_err(map_local_state_error)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })?;
        self.codex_session(id).map(|stored| stored.session)
    }

    pub fn set_codex_session_codex_id(&self, id: &str, codex_session_id: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE codex_sessions SET codex_session_id = ?2, updated_at_ms = ?3 WHERE id = ?1 AND codex_session_id IS NULL",
                params![id, codex_session_id, timestamp_ms()],
            ).map_err(map_local_state_error)?;
            Ok(())
        })
    }

    pub fn set_codex_session_provider_message(&self, id: &str, message_id: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE codex_sessions SET provider_message_id = ?2, feishu_message_id = CASE WHEN provider = 'feishu' THEN ?2 ELSE feishu_message_id END, updated_at_ms = ?3 WHERE id = ?1",
                params![id, message_id, timestamp_ms()],
            ).map_err(map_local_state_error)?;
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn set_codex_session_feishu_message(&self, id: &str, message_id: &str) -> AppResult<()> {
        self.set_codex_session_provider_message(id, message_id)
    }

    pub fn list_codex_sessions(
        &self,
        binding_id: Option<&str>,
    ) -> AppResult<Vec<StoredCodexSession>> {
        self.with_connection(|connection| {
            let sql = if binding_id.is_some() {
                "SELECT s.id, s.binding_id, s.context_id, s.provider, s.provider_bot_id, s.provider_chat_id, s.provider_message_id, b.project_name, b.project_slug, s.profile_id, p.alias, s.relay_status, s.codex_session_id, s.feishu_message_id, s.feishu_chat_id, s.started_by, s.started_at_ms, s.updated_at_ms, s.finished_at_ms, s.summary, s.last_error, s.execution_target, s.model_id, s.turn_kind, s.conversation_mode, s.goal_status, s.working_directory
                 FROM codex_sessions s JOIN collaboration_project_bindings b ON b.id = s.binding_id LEFT JOIN profiles p ON p.id = s.profile_id WHERE s.binding_id = ?1 ORDER BY s.started_at_ms DESC"
            } else {
                "SELECT s.id, s.binding_id, s.context_id, s.provider, s.provider_bot_id, s.provider_chat_id, s.provider_message_id, b.project_name, b.project_slug, s.profile_id, p.alias, s.relay_status, s.codex_session_id, s.feishu_message_id, s.feishu_chat_id, s.started_by, s.started_at_ms, s.updated_at_ms, s.finished_at_ms, s.summary, s.last_error, s.execution_target, s.model_id, s.turn_kind, s.conversation_mode, s.goal_status, s.working_directory
                 FROM codex_sessions s JOIN collaboration_project_bindings b ON b.id = s.binding_id LEFT JOIN profiles p ON p.id = s.profile_id ORDER BY s.started_at_ms DESC LIMIT 200"
            };
            let mut statement = connection.prepare(sql).map_err(map_local_state_error)?;
            let collect = |row: &rusqlite::Row<'_>| codex_session_from_row(row);
            if let Some(binding_id) = binding_id {
                let rows = statement
                    .query_map(params![binding_id], collect)
                    .map_err(map_local_state_error)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(map_local_state_error)
            } else {
                let rows = statement
                    .query_map([], collect)
                    .map_err(map_local_state_error)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(map_local_state_error)
            }
        })
    }

    pub fn codex_session(&self, id: &str) -> AppResult<StoredCodexSession> {
        self.with_connection(|connection| {
            connection.query_row(
                "SELECT s.id, s.binding_id, s.context_id, s.provider, s.provider_bot_id, s.provider_chat_id, s.provider_message_id, b.project_name, b.project_slug, s.profile_id, p.alias, s.relay_status, s.codex_session_id, s.feishu_message_id, s.feishu_chat_id, s.started_by, s.started_at_ms, s.updated_at_ms, s.finished_at_ms, s.summary, s.last_error, s.execution_target, s.model_id, s.turn_kind, s.conversation_mode, s.goal_status, s.working_directory
                 FROM codex_sessions s JOIN collaboration_project_bindings b ON b.id = s.binding_id LEFT JOIN profiles p ON p.id = s.profile_id WHERE s.id = ?1",
                params![id],
                codex_session_from_row,
            ).map_err(map_session_lookup_error)
        })
    }

    pub fn count_running_codex_sessions(&self, binding_id: Option<&str>) -> AppResult<i64> {
        self.with_connection(|connection| {
            if let Some(binding_id) = binding_id {
                connection.query_row(
                    "SELECT COUNT(*) FROM codex_sessions WHERE binding_id = ?1 AND relay_status = 'running'",
                    params![binding_id],
                    |row| row.get(0),
                ).map_err(map_local_state_error)
            } else {
                connection.query_row(
                    "SELECT COUNT(*) FROM codex_sessions WHERE relay_status = 'running'",
                    [],
                    |row| row.get(0),
                ).map_err(map_local_state_error)
            }
        })
    }

    pub fn insert_codex_session_event(&self, event: &CodexSessionEvent) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO codex_session_events(id, session_id, occurred_at_ms, event_type, content) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![event.id, event.session_id, event.occurred_at_ms, event.event_type, event.content],
            ).map_err(map_local_state_error)?;
            Ok(())
        })
    }

    #[cfg(test)]
    pub fn metrics(&self) -> AppResult<MetricsSnapshot> {
        self.gateway_performance(GatewayPerformanceInput { window_minutes: 60 })
    }

    pub fn gateway_performance(
        &self,
        input: GatewayPerformanceInput,
    ) -> AppResult<MetricsSnapshot> {
        let window_minutes = input.window_minutes.clamp(1, 7 * 24 * 60);
        let since = timestamp_ms().saturating_sub(window_minutes.saturating_mul(60_000));
        self.with_connection(|connection| {
            let (total_requests, successful_requests, failed_requests, total_latency_ms, latency_samples, estimated_tokens) = connection
                .query_row(
                    "SELECT total_requests, successful_requests, failed_requests, total_latency_ms, latency_samples, estimated_tokens FROM metrics WHERE id = 1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?)),
                )
                .map_err(|_| AppError::Internal)?;
            let mut statement = connection
                .prepare(
                    "SELECT total_latency_ms, ttfb_ms, request_bytes, response_bytes, retry_count, outcome FROM gateway_request_metrics WHERE started_at_ms >= ?1 ORDER BY started_at_ms",
                )
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map(params![since], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|_| AppError::Internal)?;
            let mut latencies = Vec::new();
            let mut ttfbs = Vec::new();
            let mut window_successes = 0_i64;
            let mut request_bytes = 0_i64;
            let mut response_bytes = 0_i64;
            let mut retry_count = 0_i64;
            for row in rows {
                let (latency, ttfb, request_size, response_size, retries, outcome) =
                    row.map_err(|_| AppError::Internal)?;
                latencies.push(latency.max(0));
                if let Some(ttfb) = ttfb {
                    ttfbs.push(ttfb.max(0));
                }
                request_bytes = request_bytes.saturating_add(request_size.max(0));
                response_bytes = response_bytes.saturating_add(response_size.max(0));
                retry_count = retry_count.saturating_add(retries.max(0));
                if outcome == "success" {
                    window_successes += 1;
                }
            }
            latencies.sort_unstable();
            ttfbs.sort_unstable();
            let window_requests = latencies.len() as i64;
            Ok(MetricsSnapshot {
                total_requests,
                successful_requests,
                failed_requests,
                average_latency_ms: if latency_samples > 0 {
                    Some(total_latency_ms / latency_samples)
                } else {
                    None
                },
                estimated_tokens,
                window_minutes,
                window_requests,
                window_success_rate: if window_requests > 0 {
                    Some(window_successes as f64 / window_requests as f64)
                } else {
                    None
                },
                requests_per_minute: window_requests as f64 / window_minutes as f64,
                latency_p50_ms: percentile(&latencies, 50),
                latency_p95_ms: percentile(&latencies, 95),
                latency_p99_ms: percentile(&latencies, 99),
                ttfb_p50_ms: percentile(&ttfbs, 50),
                ttfb_p95_ms: percentile(&ttfbs, 95),
                ttfb_p99_ms: percentile(&ttfbs, 99),
                request_bytes,
                response_bytes,
                retry_count,
                active_requests: 0,
                queued_requests: 0,
                telemetry_dropped: 0,
            })
        })
    }

    pub fn list_gateway_request_metrics(
        &self,
        input: ListGatewayRequestMetricsInput,
    ) -> AppResult<GatewayRequestMetricPage> {
        let limit = input.limit.clamp(1, 200);
        self.with_connection(|connection| {
            let cursor = input.cursor.unwrap_or(i64::MAX);
            let mut statement = connection
                .prepare(
                    "SELECT sequence, request_id, started_at_ms, route, provider, profile_id, auth_mode, stream, auth_latency_ms, queue_latency_ms, ttfb_ms, total_latency_ms, request_bytes, response_bytes, http_status, outcome, error_category, upstream_attempts, retry_count, input_tokens, output_tokens, total_tokens, upstream_response_id FROM gateway_request_metrics WHERE sequence < ?1 ORDER BY sequence DESC LIMIT 1000",
                )
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map(params![cursor], gateway_request_metric_from_row)
                .map_err(|_| AppError::Internal)?;
            let mut items = Vec::new();
            for row in rows {
                let item = row.map_err(|_| AppError::Internal)?;
                if input.profile_id.as_deref().is_some_and(|value| item.profile_id.as_deref() != Some(value))
                    || input.route.as_deref().is_some_and(|value| item.route != value)
                    || input.status.as_deref().is_some_and(|value| item.outcome != value)
                {
                    continue;
                }
                items.push(item);
                if items.len() > limit {
                    break;
                }
            }
            let next_cursor = (items.len() > limit)
                .then(|| items[limit - 1].sequence);
            items.truncate(limit);
            Ok(GatewayRequestMetricPage { items, next_cursor })
        })
    }

    pub fn record_gateway_request_metrics(
        &self,
        metrics: &[GatewayRequestMetricSummary],
    ) -> AppResult<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        self.with_connection(|connection| {
            let transaction = connection.unchecked_transaction().map_err(|_| AppError::Internal)?;
            let mut successful_requests = 0_i64;
            let mut failed_requests = 0_i64;
            let mut total_latency_ms = 0_i64;
            let mut estimated_tokens = 0_i64;
            for metric in metrics {
                transaction
                    .execute(
                        "INSERT OR REPLACE INTO gateway_request_metrics(request_id, started_at_ms, route, provider, profile_id, auth_mode, stream, auth_latency_ms, queue_latency_ms, ttfb_ms, total_latency_ms, request_bytes, response_bytes, http_status, outcome, error_category, upstream_attempts, retry_count, input_tokens, output_tokens, total_tokens, upstream_response_id) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
                        params![
                            metric.request_id, metric.started_at_ms, metric.route, metric.provider,
                            metric.profile_id, metric.auth_mode, metric.stream, metric.auth_latency_ms,
                            metric.queue_latency_ms, metric.ttfb_ms, metric.total_latency_ms,
                            metric.request_bytes, metric.response_bytes, metric.http_status,
                            metric.outcome, metric.error_category, metric.upstream_attempts,
                            metric.retry_count, metric.input_tokens, metric.output_tokens,
                            metric.total_tokens, metric.upstream_response_id,
                        ],
                    )
                    .map_err(|_| AppError::Internal)?;
                if metric.outcome == "success" {
                    successful_requests += 1;
                } else {
                    failed_requests += 1;
                }
                total_latency_ms = total_latency_ms.saturating_add(metric.total_latency_ms.max(0));
                estimated_tokens = estimated_tokens.saturating_add(metric.total_tokens.max(0));
            }
            transaction.execute(
                "UPDATE metrics SET total_requests = total_requests + ?1, successful_requests = successful_requests + ?2, failed_requests = failed_requests + ?3, total_latency_ms = total_latency_ms + ?4, latency_samples = latency_samples + ?1, estimated_tokens = estimated_tokens + ?5 WHERE id = 1",
                params![metrics.len() as i64, successful_requests, failed_requests, total_latency_ms, estimated_tokens],
            ).map_err(|_| AppError::Internal)?;
            transaction.commit().map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn prune_gateway_request_metrics(&self) -> AppResult<()> {
        let cutoff = timestamp_ms().saturating_sub(7 * 24 * 60 * 60 * 1000);
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM gateway_request_metrics WHERE started_at_ms < ?1 OR sequence NOT IN (SELECT sequence FROM gateway_request_metrics ORDER BY sequence DESC LIMIT 10000)",
                params![cutoff],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    #[cfg(test)]
    pub fn record_metric(&self, successful: bool, latency_ms: i64) -> AppResult<()> {
        self.record_metric_with_tokens(successful, latency_ms, 0)
    }

    #[cfg(test)]
    pub fn record_metric_with_tokens(
        &self,
        successful: bool,
        latency_ms: i64,
        estimated_tokens: i64,
    ) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute("UPDATE metrics SET total_requests = total_requests + 1, successful_requests = successful_requests + ?1, failed_requests = failed_requests + ?2, total_latency_ms = total_latency_ms + ?3, latency_samples = latency_samples + 1, estimated_tokens = estimated_tokens + ?4 WHERE id = 1", params![i64::from(successful), i64::from(!successful), latency_ms, estimated_tokens.max(0)]).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    #[cfg(test)]
    pub fn add_estimated_tokens(&self, estimated_tokens: i64) -> AppResult<()> {
        if estimated_tokens <= 0 {
            return Ok(());
        }
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE metrics SET estimated_tokens = estimated_tokens + ?1 WHERE id = 1",
                    params![estimated_tokens],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }
}

fn percentile(values: &[i64], percentile: usize) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values.get(index).copied()
}

fn gateway_request_metric_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<GatewayRequestMetricSummary> {
    Ok(GatewayRequestMetricSummary {
        sequence: row.get(0)?,
        request_id: row.get(1)?,
        started_at_ms: row.get(2)?,
        route: row.get(3)?,
        provider: row.get(4)?,
        profile_id: row.get(5)?,
        auth_mode: row.get(6)?,
        stream: row.get(7)?,
        auth_latency_ms: row.get(8)?,
        queue_latency_ms: row.get(9)?,
        ttfb_ms: row.get(10)?,
        total_latency_ms: row.get(11)?,
        request_bytes: row.get(12)?,
        response_bytes: row.get(13)?,
        http_status: row.get::<_, i64>(14)? as u16,
        outcome: row.get(15)?,
        error_category: row.get(16)?,
        upstream_attempts: row.get(17)?,
        retry_count: row.get(18)?,
        input_tokens: row.get(19)?,
        output_tokens: row.get(20)?,
        total_tokens: row.get(21)?,
        upstream_response_id: row.get(22)?,
    })
}

fn user_client_key_secret_ref_from_connection(
    connection: &Connection,
    id: &str,
) -> AppResult<String> {
    let secret_ref = connection
        .query_row(
            "SELECT secret_ref FROM client_keys WHERE id = ?1 AND revoked = 0",
            params![id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| AppError::Internal)?
        .ok_or(AppError::NotFound)?;
    let codex_key_ref = connection
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![GATEWAY_CODEX_CLIENT_KEY_REF_SETTING],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| AppError::Internal)?;
    if codex_key_ref.as_deref() == Some(secret_ref.as_str()) {
        return Err(AppError::Conflict);
    }
    Ok(secret_ref)
}

const COLLABORATION_CONTEXT_SELECT: &str =
    "SELECT c.id, c.scope_key, c.binding_id, b.provider, b.bot_id, bot.name, b.project_name, b.project_slug, b.working_directory, b.execution_target, b.profile_id, p.alias, c.model_id, c.memory_enabled, c.permissions_policy, c.active_codex_session_id, c.active_relay_session_id, c.goal_status, c.goal_text, c.conversation_mode, c.last_turn_at_ms, c.created_at_ms, c.updated_at_ms
     FROM collaboration_contexts c
     JOIN collaboration_project_bindings b ON b.id = c.binding_id
     JOIN collaboration_bots bot ON bot.id = b.bot_id
     LEFT JOIN profiles p ON p.id = b.profile_id";

fn feishu_binding_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeishuProjectBinding> {
    Ok(FeishuProjectBinding {
        id: row.get(0)?,
        bot_id: row.get(1)?,
        bot_name: row.get(2)?,
        project_name: row.get(3)?,
        project_slug: row.get(4)?,
        working_directory: row.get(5)?,
        profile_id: row.get(6)?,
        profile_alias: row.get(7)?,
        chat_id: row.get(8)?,
        bind_code: row.get(9)?,
        enabled: row.get(10)?,
        concurrency_limit: row.get(11)?,
        created_at_ms: row.get(12)?,
        updated_at_ms: row.get(13)?,
    })
}

fn collaboration_binding_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<CollaborationProjectBinding> {
    let provider: String = row.get(1)?;
    Ok(CollaborationProjectBinding {
        id: row.get(0)?,
        provider: parse_collaboration_provider(&provider).unwrap_or(CollaborationProvider::Feishu),
        bot_id: row.get(2)?,
        bot_name: row.get(3)?,
        project_name: row.get(4)?,
        project_slug: row.get(5)?,
        working_directory: row.get(6)?,
        profile_id: row.get(7)?,
        profile_alias: row.get(8)?,
        chat_id: row.get(9)?,
        bind_code: row.get(10)?,
        enabled: row.get(11)?,
        concurrency_limit: row.get(12)?,
        execution_target: row.get(13)?,
        model_id: row.get(14)?,
        created_at_ms: row.get(15)?,
        updated_at_ms: row.get(16)?,
    })
}

fn collaboration_context_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<CollaborationContextSummary> {
    let provider: String = row.get(3)?;
    Ok(CollaborationContextSummary {
        id: row.get(0)?,
        scope_key: row.get(1)?,
        binding_id: row.get(2)?,
        provider: parse_collaboration_provider(&provider).unwrap_or(CollaborationProvider::Feishu),
        bot_id: row.get(4)?,
        bot_name: row.get(5)?,
        project_name: row.get(6)?,
        project_slug: row.get(7)?,
        working_directory: row.get(8)?,
        execution_target: row.get(9)?,
        profile_id: row.get(10)?,
        profile_alias: row.get(11)?,
        model_id: row.get(12)?,
        memory_enabled: row.get(13)?,
        permissions_policy: row.get(14)?,
        active_codex_session_id: row.get(15)?,
        active_relay_session_id: row.get(16)?,
        goal_status: row.get(17)?,
        goal_text: row.get(18)?,
        conversation_mode: row.get(19)?,
        last_turn_at_ms: row.get(20)?,
        created_at_ms: row.get(21)?,
        updated_at_ms: row.get(22)?,
    })
}

fn system_prompt_from_config_json(value: &str) -> Option<String> {
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|config| {
            config
                .get("system_prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(ToOwned::to_owned)
        })
        .filter(|prompt| !prompt.is_empty())
}

fn map_local_state_error(_: rusqlite::Error) -> AppError {
    AppError::LocalStateUnavailable
}

fn map_session_lookup_error(error: rusqlite::Error) -> AppError {
    match error {
        rusqlite::Error::QueryReturnedNoRows => AppError::NotFound,
        _ => AppError::LocalStateUnavailable,
    }
}

fn codex_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCodexSession> {
    let working_directory: String = row.get(26)?;
    let provider: String = row.get(3)?;
    Ok(StoredCodexSession {
        session: CodexSessionSummary {
            id: row.get(0)?,
            binding_id: row.get(1)?,
            context_id: row.get(2)?,
            provider: parse_collaboration_provider(&provider)
                .unwrap_or(CollaborationProvider::Feishu),
            provider_bot_id: row.get(4)?,
            provider_chat_id: row.get(5)?,
            provider_message_id: row.get(6)?,
            project_name: row.get(7)?,
            project_slug: row.get(8)?,
            profile_id: row.get(9)?,
            profile_alias: row.get(10)?,
            relay_status: row.get(11)?,
            codex_session_id: row.get(12)?,
            feishu_message_id: row.get(13)?,
            feishu_chat_id: row.get(14)?,
            started_by: row.get(15)?,
            started_at_ms: row.get(16)?,
            updated_at_ms: row.get(17)?,
            finished_at_ms: row.get(18)?,
            summary: row.get(19)?,
            last_error: row.get(20)?,
            execution_target: row.get(21)?,
            model_id: row.get(22)?,
            turn_kind: row.get(23)?,
            conversation_mode: row.get(24)?,
            goal_status: row.get(25)?,
        },
        working_directory,
    })
}

fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn provider_name(provider: &GatewayProvider) -> &'static str {
    match provider {
        GatewayProvider::OpenAi => "openai",
        GatewayProvider::OpenAiCompatible => "openai_compatible",
        GatewayProvider::Anthropic => "anthropic",
        GatewayProvider::Gemini => "gemini",
        GatewayProvider::Ollama => "ollama",
    }
}

fn wire_api_name(wire_api: &GatewayWireApi) -> &'static str {
    match wire_api {
        GatewayWireApi::Responses => "responses",
        GatewayWireApi::ChatCompletions => "chat_completions",
    }
}

fn parse_provider(value: &str) -> Option<GatewayProvider> {
    match value {
        "openai" | "open_ai" => Some(GatewayProvider::OpenAi),
        "openai_compatible" | "open_ai_compatible" => Some(GatewayProvider::OpenAiCompatible),
        "anthropic" => Some(GatewayProvider::Anthropic),
        "gemini" => Some(GatewayProvider::Gemini),
        "ollama" => Some(GatewayProvider::Ollama),
        _ => None,
    }
}

fn parse_wire_api(value: &str) -> Option<GatewayWireApi> {
    match value {
        "responses" => Some(GatewayWireApi::Responses),
        "chat_completions" => Some(GatewayWireApi::ChatCompletions),
        _ => None,
    }
}

fn stored_or_identity_model_mappings(
    models: &[String],
    mappings: Vec<GatewayModelMapping>,
) -> Vec<GatewayModelMapping> {
    if !mappings.is_empty() {
        return mappings;
    }
    models
        .iter()
        .map(|model| GatewayModelMapping {
            model: model.clone(),
            upstream_model: model.clone(),
            display_name: None,
            context_window: None,
        })
        .collect()
}

fn profile_kind_name(kind: &ProfileKind) -> &'static str {
    match kind {
        ProfileKind::ApiKey => "api_key",
        ProfileKind::CodexOauth => "codex_oauth",
    }
}

fn auth_mode_name(mode: &CodexAuthMode) -> &'static str {
    match mode {
        CodexAuthMode::OAuth => "oauth",
        CodexAuthMode::AgentIdentity => "agent_identity",
        CodexAuthMode::PersonalAccessToken => "personal_access_token",
    }
}

fn parse_auth_mode(value: &str) -> Option<CodexAuthMode> {
    match value {
        "oauth" => Some(CodexAuthMode::OAuth),
        "agent_identity" => Some(CodexAuthMode::AgentIdentity),
        "personal_access_token" => Some(CodexAuthMode::PersonalAccessToken),
        _ => None,
    }
}

fn collaboration_provider_name(provider: &CollaborationProvider) -> &'static str {
    match provider {
        CollaborationProvider::Feishu => "feishu",
        CollaborationProvider::Qq => "qq",
        CollaborationProvider::Wecom => "wecom",
        CollaborationProvider::Discord => "discord",
        CollaborationProvider::Telegram => "telegram",
    }
}

fn parse_collaboration_provider(value: &str) -> Option<CollaborationProvider> {
    match value {
        "feishu" => Some(CollaborationProvider::Feishu),
        "qq" => Some(CollaborationProvider::Qq),
        "wecom" => Some(CollaborationProvider::Wecom),
        "discord" => Some(CollaborationProvider::Discord),
        "telegram" => Some(CollaborationProvider::Telegram),
        _ => None,
    }
}

fn profile_columns(connection: &Connection) -> AppResult<Vec<String>> {
    table_columns(connection, "profiles")
}

fn table_columns(connection: &Connection, table: &str) -> AppResult<Vec<String>> {
    connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|_| AppError::Internal)
}

fn table_column_notnull(connection: &Connection, table: &str, column: &str) -> AppResult<bool> {
    connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut statement| {
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
            })?;
            for row in rows {
                let (name, notnull) = row?;
                if name == column {
                    return Ok(notnull != 0);
                }
            }
            Ok(false)
        })
        .map_err(|_| AppError::Internal)
}

fn make_collaboration_profile_columns_nullable(connection: &Connection) -> AppResult<()> {
    if table_column_notnull(connection, "collaboration_project_bindings", "profile_id")? {
        connection
            .execute_batch(
                "
                PRAGMA foreign_keys = OFF;
                ALTER TABLE collaboration_project_bindings RENAME TO collaboration_project_bindings_notnull_profile;
                CREATE TABLE collaboration_project_bindings (
                  id TEXT PRIMARY KEY,
                  provider TEXT NOT NULL,
                  bot_id TEXT NOT NULL,
                  project_name TEXT NOT NULL,
                  project_slug TEXT NOT NULL,
                  working_directory TEXT NOT NULL,
                  profile_id TEXT,
                  chat_id TEXT,
                  bind_code TEXT NOT NULL UNIQUE,
                  enabled INTEGER NOT NULL,
                  concurrency_limit INTEGER NOT NULL,
                  execution_target TEXT NOT NULL DEFAULT 'profile',
                  model_id TEXT,
                  created_at_ms INTEGER NOT NULL,
                  updated_at_ms INTEGER NOT NULL,
                  FOREIGN KEY(bot_id) REFERENCES collaboration_bots(id) ON DELETE CASCADE,
                  FOREIGN KEY(profile_id) REFERENCES profiles(id) ON DELETE CASCADE
                );
                INSERT INTO collaboration_project_bindings(id, provider, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, execution_target, model_id, created_at_ms, updated_at_ms)
                SELECT id, provider, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, execution_target, model_id, created_at_ms, updated_at_ms
                FROM collaboration_project_bindings_notnull_profile;
                DROP TABLE collaboration_project_bindings_notnull_profile;
                CREATE INDEX IF NOT EXISTS idx_collaboration_project_bindings_bot_chat
                  ON collaboration_project_bindings(provider, bot_id, chat_id);
                PRAGMA foreign_keys = ON;
                ",
            )
            .map_err(|_| AppError::Internal)?;
    }
    if table_column_notnull(connection, "codex_sessions", "profile_id")? {
        connection
            .execute_batch(
                "
                PRAGMA foreign_keys = OFF;
                ALTER TABLE codex_sessions RENAME TO codex_sessions_notnull_profile;
                CREATE TABLE codex_sessions (
                  id TEXT PRIMARY KEY,
                  binding_id TEXT NOT NULL,
                  context_id TEXT,
                  profile_id TEXT,
                  provider TEXT NOT NULL DEFAULT 'feishu',
                  provider_bot_id TEXT,
                  provider_chat_id TEXT,
                  provider_message_id TEXT,
                  relay_status TEXT NOT NULL,
                  codex_session_id TEXT,
                  feishu_message_id TEXT,
                  feishu_chat_id TEXT,
                  started_by TEXT,
                  started_at_ms INTEGER NOT NULL,
                  updated_at_ms INTEGER NOT NULL,
                  finished_at_ms INTEGER,
                  summary TEXT,
                  last_error TEXT,
                  execution_target TEXT NOT NULL DEFAULT 'profile',
                  model_id TEXT,
                  turn_kind TEXT NOT NULL DEFAULT 'run',
                  conversation_mode TEXT NOT NULL DEFAULT 'default',
                  goal_status TEXT,
                  working_directory TEXT NOT NULL,
                  FOREIGN KEY(binding_id) REFERENCES collaboration_project_bindings(id) ON DELETE CASCADE,
                  FOREIGN KEY(context_id) REFERENCES collaboration_contexts(id) ON DELETE SET NULL
                );
                INSERT INTO codex_sessions(id, binding_id, context_id, profile_id, provider, provider_bot_id, provider_chat_id, provider_message_id, relay_status, codex_session_id, feishu_message_id, feishu_chat_id, started_by, started_at_ms, updated_at_ms, finished_at_ms, summary, last_error, execution_target, model_id, turn_kind, conversation_mode, goal_status, working_directory)
                SELECT id, binding_id, NULL, profile_id, provider, provider_bot_id, provider_chat_id, provider_message_id, relay_status, codex_session_id, feishu_message_id, feishu_chat_id, started_by, started_at_ms, updated_at_ms, finished_at_ms, summary, last_error, execution_target, model_id, 'run', 'default', NULL, working_directory
                FROM codex_sessions_notnull_profile;
                DROP TABLE codex_sessions_notnull_profile;
                CREATE INDEX IF NOT EXISTS idx_codex_sessions_binding_status
                  ON codex_sessions(binding_id, relay_status);
                CREATE INDEX IF NOT EXISTS idx_codex_sessions_context_status
                  ON codex_sessions(context_id, relay_status);
                PRAGMA foreign_keys = ON;
                ",
            )
            .map_err(|_| AppError::Internal)?;
    }
    Ok(())
}

fn migrate_feishu_to_collaboration(connection: &Connection) -> AppResult<()> {
    let feishu_exists: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'feishu_bots'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| AppError::Internal)?;
    if feishu_exists.is_none() {
        return Ok(());
    }
    {
        let mut statement = connection
            .prepare("SELECT id, name, app_id, app_id_mask, app_secret_ref, enabled, connection_status, last_error, updated_at_ms FROM feishu_bots")
            .map_err(|_| AppError::Internal)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(|_| AppError::Internal)?;
        for row in rows {
            let (
                id,
                name,
                app_id,
                app_id_mask,
                app_secret_ref,
                enabled,
                connection_status,
                last_error,
                updated_at_ms,
            ) = row.map_err(|_| AppError::Internal)?;
            let config_json = serde_json::json!({"app_id": app_id}).to_string();
            let secret_refs_json = serde_json::json!({"app_secret": app_secret_ref}).to_string();
            connection
                .execute(
                    "INSERT OR IGNORE INTO collaboration_bots(id, provider, name, enabled, credential_mask, config_summary, config_json, secret_refs_json, connection_status, last_error, callback_public_url, updated_at_ms)
                     VALUES(?1, 'feishu', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
                    params![
                        id,
                        name,
                        enabled,
                        app_id_mask,
                        "飞书自建应用",
                        config_json,
                        secret_refs_json,
                        connection_status,
                        last_error,
                        updated_at_ms,
                    ],
                )
                .map_err(|_| AppError::Internal)?;
        }
    }
    {
        let mut statement = connection
            .prepare("SELECT id, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, created_at_ms, updated_at_ms FROM feishu_project_bindings")
            .map_err(|_| AppError::Internal)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, bool>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            })
            .map_err(|_| AppError::Internal)?;
        for row in rows {
            let (
                id,
                bot_id,
                project_name,
                project_slug,
                working_directory,
                profile_id,
                chat_id,
                bind_code,
                enabled,
                concurrency_limit,
                created_at_ms,
                updated_at_ms,
            ) = row.map_err(|_| AppError::Internal)?;
            connection
                .execute(
                    "INSERT OR IGNORE INTO collaboration_project_bindings(id, provider, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, created_at_ms, updated_at_ms)
                     VALUES(?1, 'feishu', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        id,
                        bot_id,
                        project_name,
                        project_slug,
                        working_directory,
                        profile_id,
                        chat_id,
                        bind_code,
                        enabled,
                        concurrency_limit,
                        created_at_ms,
                        updated_at_ms,
                    ],
                )
                .map_err(|_| AppError::Internal)?;
        }
    }
    connection
        .execute(
            "UPDATE codex_sessions
             SET provider = COALESCE(NULLIF(provider, ''), 'feishu'),
                 provider_chat_id = COALESCE(provider_chat_id, feishu_chat_id),
                 provider_message_id = COALESCE(provider_message_id, feishu_message_id),
                 provider_bot_id = COALESCE(provider_bot_id, (SELECT bot_id FROM collaboration_project_bindings b WHERE b.id = codex_sessions.binding_id))
             WHERE provider = 'feishu' OR provider IS NULL",
            [],
        )
        .map_err(|_| AppError::Internal)?;
    Ok(())
}

fn account_summary(
    display_name: Option<String>,
    email: Option<String>,
    account_id: Option<String>,
    updated_at_ms: Option<i64>,
    quota_json: Option<String>,
    subscription_json: Option<String>,
) -> Option<ProfileAccountSummary> {
    updated_at_ms.map(|updated_at_ms| ProfileAccountSummary {
        display_name,
        email,
        account_id,
        updated_at_ms,
        quota: quota_json
            .and_then(|quota| serde_json::from_str(&quota).ok())
            .unwrap_or_else(unavailable_quota),
        subscription: subscription_json
            .and_then(|subscription| serde_json::from_str(&subscription).ok())
            .unwrap_or_else(unavailable_subscription),
    })
}

fn unavailable_quota() -> ProfileQuota {
    ProfileQuota {
        status: "unavailable".to_owned(),
        message: "额度尚未同步。".to_owned(),
        source: None,
        synced_at_ms: None,
        last_attempt_at_ms: 0,
        last_error: None,
        primary: None,
        secondary: None,
        buckets: Vec::new(),
        rate_limit_reached_type: None,
    }
}

fn unavailable_subscription() -> ProfileSubscription {
    ProfileSubscription {
        status: "unavailable".to_owned(),
        plan_type: None,
        period_ends_at_ms: None,
        will_renew: None,
        source: None,
        synced_at_ms: None,
        last_attempt_at_ms: 0,
        last_error: None,
    }
}

fn parse_profile_kind(value: &str) -> Option<ProfileKind> {
    match value {
        "api_key" => Some(ProfileKind::ApiKey),
        "codex_oauth" => Some(ProfileKind::CodexOauth),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::{
        profile_columns, table_columns, timestamp_ms, Repository, StoredCodexSession, StoredProfile,
    };
    use crate::{
        domain::{
            AppUpdateChannel, AppUpdateSettings, CodexSessionEvent, CodexSessionSummary,
            CollaborationProvider, CollaborationSummary, DesktopWorkspaceMode, GatewayProvider,
            GatewayRequestMetricSummary, GatewayWireApi, MaskedClientKey, MaskedProfile,
            ProfileKind, GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
            GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING,
        },
        error::AppError,
        profiles,
        secrets::MemorySecretStore,
    };
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

    fn stored_profile(id: &str, kind: ProfileKind, health: &str) -> StoredProfile {
        StoredProfile {
            profile: MaskedProfile {
                id: id.into(),
                alias: id.into(),
                kind,
                base_url: Some("https://api.example.com/v1".into()),
                provider: GatewayProvider::OpenAi,
                wire_api: GatewayWireApi::Responses,
                enabled: true,
                in_pool: true,
                priority: 0,
                weight: 1,
                models: vec!["gpt-5".into()],
                model_mappings: Vec::new(),
                health: health.into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
                max_concurrency: 4,
                max_queue_depth: 8,
                queue_timeout_ms: 15_000,
            },
            secret_ref: Some(format!("profile:{id}:credential")),
            credential_fingerprint: None,
        }
    }

    fn stored_codex_session(id: &str) -> StoredCodexSession {
        StoredCodexSession {
            session: CodexSessionSummary {
                id: id.into(),
                binding_id: "binding-1".into(),
                context_id: None,
                provider: CollaborationProvider::Feishu,
                provider_bot_id: Some("bot-1".into()),
                provider_chat_id: Some("chat-1".into()),
                provider_message_id: None,
                project_name: "Relay".into(),
                project_slug: "relay".into(),
                profile_id: Some("profile-1".into()),
                profile_alias: Some("工作账号".into()),
                relay_status: "running".into(),
                codex_session_id: None,
                feishu_message_id: None,
                feishu_chat_id: Some("chat-1".into()),
                started_by: Some("sender-1".into()),
                started_at_ms: 1,
                updated_at_ms: 1,
                finished_at_ms: None,
                summary: Some("任务已启动。".into()),
                last_error: None,
                execution_target: "profile".into(),
                model_id: None,
                turn_kind: "run".into(),
                conversation_mode: "default".into(),
                goal_status: None,
            },
            working_directory: "/tmp".into(),
        }
    }

    #[test]
    fn codex_session_distinguishes_missing_from_unreadable_local_state() {
        let repository = Repository::memory();

        let missing = repository.codex_session("missing").unwrap_err();
        assert!(matches!(missing, AppError::NotFound));

        repository
            .with_connection(|connection| {
                connection
                    .execute("DROP TABLE codex_sessions", [])
                    .map_err(|_| AppError::Internal)?;
                Ok(())
            })
            .unwrap();

        let unreadable = repository.codex_session("missing").unwrap_err();
        assert!(matches!(unreadable, AppError::LocalStateUnavailable));
    }

    #[test]
    fn list_codex_sessions_reports_unreadable_local_state() {
        let repository = Repository::memory();
        repository
            .with_connection(|connection| {
                connection
                    .execute("DROP TABLE codex_sessions", [])
                    .map_err(|_| AppError::Internal)?;
                Ok(())
            })
            .unwrap();

        let error = repository.list_codex_sessions(None).unwrap_err();
        assert!(matches!(error, AppError::LocalStateUnavailable));
    }

    #[test]
    fn insert_codex_session_reports_unreadable_local_state() {
        let repository = Repository::memory();
        repository
            .with_connection(|connection| {
                connection
                    .execute("DROP TABLE codex_sessions", [])
                    .map_err(|_| AppError::Internal)?;
                Ok(())
            })
            .unwrap();

        let error = repository
            .insert_codex_session(&stored_codex_session("session-1"))
            .unwrap_err();
        assert!(matches!(error, AppError::LocalStateUnavailable));
    }

    #[test]
    fn insert_codex_session_event_reports_unreadable_local_state() {
        let repository = Repository::memory();
        repository
            .with_connection(|connection| {
                connection
                    .execute("DROP TABLE codex_session_events", [])
                    .map_err(|_| AppError::Internal)?;
                Ok(())
            })
            .unwrap();

        let error = repository
            .insert_codex_session_event(&CodexSessionEvent {
                id: "event-1".into(),
                session_id: "session-1".into(),
                occurred_at_ms: 1,
                event_type: "cancelled".into(),
                content: "任务已取消。".into(),
            })
            .unwrap_err();
        assert!(matches!(error, AppError::LocalStateUnavailable));
    }

    #[test]
    fn collaboration_schema_includes_execution_snapshot_columns() {
        let repository = Repository::memory();
        repository
            .with_connection(|connection| {
                let binding_columns = table_columns(connection, "collaboration_project_bindings")?;
                assert!(binding_columns.contains(&"execution_target".to_owned()));
                assert!(binding_columns.contains(&"model_id".to_owned()));
                let session_columns = table_columns(connection, "codex_sessions")?;
                assert!(session_columns.contains(&"execution_target".to_owned()));
                assert!(session_columns.contains(&"model_id".to_owned()));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn deleting_oauth_profile_clears_api_unlock_bindings_and_gateway_settings() {
        let repository = Repository::memory();
        let oauth = stored_profile("oauth-login", ProfileKind::CodexOauth, "healthy");
        repository.insert_profile(&oauth).unwrap();
        let mut api = stored_profile("api-profile", ProfileKind::ApiKey, "healthy");
        api.profile.codex_oauth_profile_id = Some("oauth-login".to_owned());
        repository.insert_profile(&api).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING, "oauth-login")
            .unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "oauth-login")
            .unwrap();

        let secret_ref = repository.delete_profile("oauth-login").unwrap();

        assert_eq!(
            secret_ref.as_deref(),
            Some("profile:oauth-login:credential")
        );
        assert!(repository
            .profile("api-profile")
            .unwrap()
            .profile
            .codex_oauth_profile_id
            .is_none());
        assert!(repository
            .setting(GATEWAY_CODEX_OAUTH_PROFILE_ID_SETTING)
            .unwrap()
            .is_none());
        assert!(repository
            .setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)
            .unwrap()
            .is_none());
    }

    #[test]
    fn migrates_legacy_profiles_without_account_columns() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE profiles (
                  id TEXT PRIMARY KEY,
                  alias TEXT NOT NULL UNIQUE,
                  kind TEXT NOT NULL,
                  base_url TEXT,
                  enabled INTEGER NOT NULL,
                  in_pool INTEGER NOT NULL,
                  priority INTEGER NOT NULL,
                  weight INTEGER NOT NULL,
                  models_json TEXT NOT NULL,
                  health TEXT NOT NULL,
                  cooldown_until_ms INTEGER,
                  secret_ref TEXT
                );
                INSERT INTO profiles VALUES ('legacy', 'Legacy', 'codex_oauth', NULL, 1, 0, 0, 1, '[]', 'unknown', NULL, 'profile:legacy:oauth');
                INSERT INTO profiles VALUES ('legacy-api', 'Legacy API', 'api_key', 'https://api.example.com/v1', 1, 1, 0, 1, '[\"model-a\"]', 'healthy', NULL, 'profile:legacy-api:credential');
                ",
            )
            .unwrap();
        let repository = Repository {
            connection: Mutex::new(connection),
        };

        repository.migrate().unwrap();

        let columns = repository
            .with_connection(profile_columns)
            .expect("migrated columns must be readable");
        assert!(columns.contains(&"account_email".to_owned()));
        assert!(columns.contains(&"account_subscription_json".to_owned()));
        assert!(columns.contains(&"wire_api".to_owned()));
        assert!(columns.contains(&"model_mappings_json".to_owned()));
        assert!(columns.contains(&"codex_oauth_profile_id".to_owned()));
        assert!(columns.contains(&"validation_status".to_owned()));
        assert!(columns.contains(&"validated_at_ms".to_owned()));
        assert!(columns.contains(&"validation_message".to_owned()));
        assert!(columns.contains(&"max_concurrency".to_owned()));
        assert!(columns.contains(&"max_queue_depth".to_owned()));
        assert!(columns.contains(&"queue_timeout_ms".to_owned()));
        let api_profile = repository.profile("legacy-api").unwrap().profile;
        assert_eq!(api_profile.wire_api, GatewayWireApi::Responses);
        assert_eq!(api_profile.model_mappings.len(), 1);
        assert_eq!(api_profile.model_mappings[0].model, "model-a");
        assert_eq!(api_profile.model_mappings[0].upstream_model, "model-a");
        assert_eq!(api_profile.validation_status, "unknown");
        assert!(api_profile.validated_at_ms.is_none());
        assert!(api_profile.validation_message.is_none());
        assert_eq!(api_profile.max_concurrency, 4);
        assert_eq!(api_profile.max_queue_depth, 8);
        assert_eq!(api_profile.queue_timeout_ms, 15_000);
        let profile = repository.profile("legacy").unwrap().profile;
        assert!(profile.account.is_none());
        assert!(profile.credential_configured);
        assert_eq!(profile.validation_status, "unknown");
    }

    #[test]
    fn profile_validation_fields_round_trip_for_all_states() {
        let repository = Repository::memory();
        let mut stored = stored_profile("validation", ProfileKind::CodexOauth, "healthy");
        repository.insert_profile(&stored).unwrap();

        for (index, status) in ["valid", "invalid", "unknown"].into_iter().enumerate() {
            stored.profile.validation_status = status.to_owned();
            stored.profile.validated_at_ms = Some(1_700_000_000_000 + index as i64);
            stored.profile.validation_message = Some(format!("{status} message"));
            repository.update_profile(&stored).unwrap();

            let persisted = repository.profile("validation").unwrap().profile;
            assert_eq!(persisted.validation_status, status);
            assert_eq!(
                persisted.validated_at_ms,
                Some(1_700_000_000_000 + index as i64)
            );
            assert_eq!(
                persisted.validation_message,
                Some(format!("{status} message"))
            );
        }
    }

    #[test]
    fn returns_no_average_latency_before_any_metrics_are_recorded() {
        let repository = Repository::memory();

        let metrics = repository
            .metrics()
            .expect("default metrics must be readable");

        assert_eq!(metrics.total_requests, 0);
        assert_eq!(metrics.average_latency_ms, None);
    }

    #[test]
    fn migrates_legacy_feishu_collaboration_data() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE profiles (
                  id TEXT PRIMARY KEY,
                  alias TEXT NOT NULL UNIQUE,
                  kind TEXT NOT NULL,
                  base_url TEXT,
                  enabled INTEGER NOT NULL,
                  in_pool INTEGER NOT NULL,
                  priority INTEGER NOT NULL,
                  weight INTEGER NOT NULL,
                  models_json TEXT NOT NULL,
                  health TEXT NOT NULL,
                  cooldown_until_ms INTEGER,
                  secret_ref TEXT
                );
                CREATE TABLE feishu_bots (
                  id TEXT PRIMARY KEY,
                  name TEXT NOT NULL,
                  app_id TEXT NOT NULL,
                  app_id_mask TEXT NOT NULL,
                  app_secret_ref TEXT NOT NULL,
                  enabled INTEGER NOT NULL,
                  connection_status TEXT NOT NULL,
                  last_error TEXT,
                  updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE feishu_project_bindings (
                  id TEXT PRIMARY KEY,
                  bot_id TEXT NOT NULL,
                  project_name TEXT NOT NULL,
                  project_slug TEXT NOT NULL,
                  working_directory TEXT NOT NULL,
                  profile_id TEXT,
                  chat_id TEXT,
                  bind_code TEXT NOT NULL UNIQUE,
                  enabled INTEGER NOT NULL,
                  concurrency_limit INTEGER NOT NULL,
                  created_at_ms INTEGER NOT NULL,
                  updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE codex_sessions (
                  id TEXT PRIMARY KEY,
                  binding_id TEXT NOT NULL,
                  profile_id TEXT NOT NULL,
                  relay_status TEXT NOT NULL,
                  codex_session_id TEXT,
                  feishu_message_id TEXT,
                  feishu_chat_id TEXT,
                  started_by TEXT,
                  started_at_ms INTEGER NOT NULL,
                  updated_at_ms INTEGER NOT NULL,
                  finished_at_ms INTEGER,
                  summary TEXT,
                  last_error TEXT,
                  working_directory TEXT NOT NULL
                );
                INSERT INTO profiles VALUES ('profile', 'Profile', 'codex_oauth', NULL, 1, 0, 0, 1, '[]', 'unknown', NULL, 'profile:profile:oauth');
                INSERT INTO feishu_bots VALUES ('bot', '飞书助手', 'cli_123456', 'cli_••3456', 'feishu-bot:bot:app_secret', 1, 'connected', NULL, 10);
                INSERT INTO feishu_project_bindings VALUES ('binding', 'bot', 'Relay', 'relay', '/tmp/relay', 'profile', 'chat-1', 'ABC123', 1, 2, 11, 12);
                INSERT INTO codex_sessions VALUES ('session', 'binding', 'profile', 'completed', 'codex-session', 'message-1', 'chat-1', 'user', 13, 14, 15, 'done', NULL, '/tmp/relay');
                ",
            )
            .unwrap();
        let repository = Repository {
            connection: Mutex::new(connection),
        };

        repository.migrate().unwrap();

        let bot = repository.collaboration_bot("bot").unwrap().bot;
        assert_eq!(bot.provider, CollaborationProvider::Feishu);
        assert_eq!(bot.credential_mask, "cli_••3456");
        let binding = repository.collaboration_project_binding("binding").unwrap();
        assert_eq!(binding.provider, CollaborationProvider::Feishu);
        assert_eq!(binding.bot_id, "bot");
        assert_eq!(binding.chat_id.as_deref(), Some("chat-1"));
        let session = repository.codex_session("session").unwrap().session;
        assert_eq!(session.provider, CollaborationProvider::Feishu);
        assert_eq!(session.provider_bot_id.as_deref(), Some("bot"));
        assert_eq!(session.provider_chat_id.as_deref(), Some("chat-1"));
        assert_eq!(session.provider_message_id.as_deref(), Some("message-1"));
        assert_eq!(
            repository.collaboration_summary().unwrap(),
            CollaborationSummary {
                enabled_bots: 1,
                bound_chats: 1,
                active_sessions: 0,
            }
        );
    }

    #[test]
    fn reads_and_drops_legacy_notification_channel_refs() {
        let repository = Repository::memory();
        repository
            .with_connection(|connection| {
                connection
                    .execute_batch(
                        "
                        CREATE TABLE channels (
                          id TEXT PRIMARY KEY,
                          name TEXT NOT NULL,
                          kind TEXT NOT NULL,
                          enabled INTEGER NOT NULL,
                          endpoint_mask TEXT NOT NULL,
                          endpoint_ref TEXT NOT NULL,
                          signing_ref TEXT,
                          last_status TEXT NOT NULL
                        );
                        INSERT INTO channels(id, name, kind, enabled, endpoint_mask, endpoint_ref, signing_ref, last_status)
                        VALUES ('one', 'Legacy', 'feishu', 1, 'https://example.com/••••', 'channel:one:endpoint', 'channel:one:signing', 'delivered');
                        ",
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap();

        let refs = repository.legacy_channel_secret_refs().unwrap();
        assert_eq!(
            refs,
            vec![
                "channel:one:endpoint".to_owned(),
                "channel:one:signing".to_owned()
            ]
        );

        repository.drop_legacy_channels().unwrap();
        assert!(repository.legacy_channel_secret_refs().unwrap().is_empty());
    }

    #[test]
    fn calculates_average_latency_after_metrics_are_recorded() {
        let repository = Repository::memory();
        repository
            .record_metric(true, 120)
            .expect("first metric must record");
        repository
            .record_metric(false, 80)
            .expect("second metric must record");

        let metrics = repository
            .metrics()
            .expect("recorded metrics must be readable");

        assert_eq!(metrics.total_requests, 2);
        assert_eq!(metrics.successful_requests, 1);
        assert_eq!(metrics.failed_requests, 1);
        assert_eq!(metrics.average_latency_ms, Some(100));
    }

    #[test]
    fn gateway_metrics_accumulate_estimated_tokens() {
        let repository = Repository::memory();
        repository
            .record_metric_with_tokens(true, 50, 12)
            .expect("metric with tokens must record");
        repository
            .add_estimated_tokens(8)
            .expect("stream tokens must accumulate");

        let metrics = repository.metrics().unwrap();

        assert_eq!(metrics.total_requests, 1);
        assert_eq!(metrics.successful_requests, 1);
        assert_eq!(metrics.estimated_tokens, 20);
    }

    #[test]
    fn gateway_request_metrics_batch_updates_aggregate_once_per_request() {
        let repository = Repository::memory();
        let make_metric = |id: &str, outcome: &str, tokens: i64| GatewayRequestMetricSummary {
            sequence: 0,
            request_id: id.to_owned(),
            started_at_ms: timestamp_ms(),
            route: "responses".to_owned(),
            provider: "openai_compatible".to_owned(),
            profile_id: Some("zeron".to_owned()),
            auth_mode: "oauth".to_owned(),
            stream: true,
            auth_latency_ms: 1,
            queue_latency_ms: 2,
            ttfb_ms: Some(3),
            total_latency_ms: 10,
            request_bytes: 100,
            response_bytes: 200,
            http_status: if outcome == "success" { 200 } else { 502 },
            outcome: outcome.to_owned(),
            error_category: (outcome != "success").then(|| "upstream".to_owned()),
            upstream_attempts: 1,
            retry_count: 0,
            input_tokens: tokens / 2,
            output_tokens: tokens - tokens / 2,
            total_tokens: tokens,
            upstream_response_id: None,
        };
        repository
            .record_gateway_request_metrics(&[
                make_metric("request-a", "success", 12),
                make_metric("request-b", "failed", 8),
            ])
            .unwrap();

        let metrics = repository.metrics().unwrap();
        assert_eq!(metrics.total_requests, 2);
        assert_eq!(metrics.successful_requests, 1);
        assert_eq!(metrics.failed_requests, 1);
        assert_eq!(metrics.estimated_tokens, 20);
        assert_eq!(metrics.window_requests, 2);
        assert_eq!(metrics.latency_p50_ms, Some(10));
        assert_eq!(metrics.ttfb_p95_ms, Some(3));
    }

    #[test]
    fn gateway_request_metrics_prune_by_age_and_count_without_resetting_aggregates() {
        let repository = Repository::memory();
        let now = timestamp_ms();
        let metrics = (0..10_005)
            .map(|index| GatewayRequestMetricSummary {
                sequence: 0,
                request_id: format!("retention-{index}"),
                started_at_ms: if index == 10_004 {
                    now - 8 * 24 * 60 * 60 * 1000
                } else {
                    now
                },
                route: "responses".to_owned(),
                provider: "openai_compatible".to_owned(),
                profile_id: Some("zeron".to_owned()),
                auth_mode: "oauth".to_owned(),
                stream: false,
                auth_latency_ms: 1,
                queue_latency_ms: 0,
                ttfb_ms: Some(2),
                total_latency_ms: 3,
                request_bytes: 10,
                response_bytes: 20,
                http_status: 200,
                outcome: "success".to_owned(),
                error_category: None,
                upstream_attempts: 1,
                retry_count: 0,
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                upstream_response_id: None,
            })
            .collect::<Vec<_>>();
        repository.record_gateway_request_metrics(&metrics).unwrap();
        repository.prune_gateway_request_metrics().unwrap();

        let retained = repository
            .with_connection(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM gateway_request_metrics", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(|_| AppError::Internal)
            })
            .unwrap();
        assert_eq!(retained, 9_999);
        assert_eq!(repository.metrics().unwrap().total_requests, 10_005);
    }

    #[test]
    fn client_key_material_can_rotate_only_user_keys() {
        let repository = Repository::memory();
        insert_test_client_key(&repository, "user", "用户 Key", "client-key:user");
        insert_test_client_key(
            &repository,
            "codex",
            "Codex CLI Gateway",
            "client-key:codex",
        );
        repository
            .set_setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, "client-key:codex")
            .unwrap();

        let secret_ref = repository
            .update_client_key_material("user", "new-hash", "crl_••••next")
            .unwrap();

        assert_eq!(secret_ref, "client-key:user");
        let user = repository
            .list_client_keys()
            .unwrap()
            .into_iter()
            .find(|key| key.id == "user")
            .unwrap();
        assert_eq!(user.masked_value, "crl_••••next");
        assert!(matches!(
            repository.update_client_key_material("codex", "hash", "crl_••••deny"),
            Err(crate::error::AppError::Conflict)
        ));
    }

    #[test]
    fn client_key_listing_hides_revoked_keys_and_marks_codex_managed_key() {
        let repository = Repository::memory();
        insert_test_client_key(&repository, "user", "用户 Key", "client-key:user");
        insert_test_client_key(
            &repository,
            "codex",
            "Codex CLI Gateway",
            "client-key:codex",
        );
        insert_test_client_key(&repository, "revoked", "已撤销", "client-key:revoked");
        repository.revoke_client_key("revoked").unwrap();
        repository
            .set_setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, "client-key:codex")
            .unwrap();

        let keys = repository.list_client_keys().unwrap();

        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|key| !key.revoked));
        assert!(keys.iter().all(|key| key.id != "revoked"));
        let codex = keys.iter().find(|key| key.id == "codex").unwrap();
        assert_eq!(codex.managed_by, "codex_gateway");
        assert!(!codex.can_revoke);
        let user = keys.iter().find(|key| key.id == "user").unwrap();
        assert_eq!(user.managed_by, "user");
        assert!(user.can_revoke);
    }

    #[test]
    fn client_key_revoke_rejects_codex_managed_key() {
        let repository = Repository::memory();
        insert_test_client_key(
            &repository,
            "codex",
            "Codex CLI Gateway",
            "client-key:codex",
        );
        repository
            .set_setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, "client-key:codex")
            .unwrap();

        assert!(matches!(
            repository.revoke_client_key("codex"),
            Err(crate::error::AppError::Conflict)
        ));
        assert_eq!(repository.list_client_keys().unwrap().len(), 1);
    }

    #[test]
    fn gateway_status_counts_healthy_api_and_opted_in_oauth_pool_members() {
        let repository = Repository::memory();
        let stored_profile = |id: &str, kind: ProfileKind, health: &str| StoredProfile {
            profile: MaskedProfile {
                id: id.into(),
                alias: id.into(),
                kind,
                base_url: Some("https://api.example.com/v1".into()),
                provider: GatewayProvider::OpenAi,
                wire_api: GatewayWireApi::Responses,
                enabled: true,
                in_pool: true,
                priority: 0,
                weight: 1,
                models: vec!["gpt-5".into()],
                model_mappings: Vec::new(),
                health: health.into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
                max_concurrency: 4,
                max_queue_depth: 8,
                queue_timeout_ms: 15_000,
            },
            secret_ref: Some(format!("profile:{id}:credential")),
            credential_fingerprint: None,
        };
        repository
            .insert_profile(&stored_profile(
                "api-profile",
                ProfileKind::ApiKey,
                "healthy",
            ))
            .unwrap();
        repository
            .insert_profile(&stored_profile(
                "oauth-unknown-profile",
                ProfileKind::CodexOauth,
                "unknown",
            ))
            .unwrap();
        repository
            .insert_profile(&stored_profile(
                "oauth-unhealthy-profile",
                ProfileKind::CodexOauth,
                "unhealthy",
            ))
            .unwrap();
        repository
            .insert_profile(&stored_profile(
                "oauth-reauth-profile",
                ProfileKind::CodexOauth,
                "reauthorization_required",
            ))
            .unwrap();
        let mut missing_credential = stored_profile(
            "oauth-missing-credential",
            ProfileKind::CodexOauth,
            "unknown",
        );
        missing_credential.profile.credential_configured = false;
        repository.insert_profile(&missing_credential).unwrap();
        let mut cooling_profile =
            stored_profile("oauth-cooling-profile", ProfileKind::CodexOauth, "unknown");
        cooling_profile.profile.cooldown_until_ms = Some(i64::MAX);
        repository.insert_profile(&cooling_profile).unwrap();
        let mut cooldown_expired_profile = stored_profile(
            "oauth-cooldown-expired-profile",
            ProfileKind::CodexOauth,
            "unknown",
        );
        cooldown_expired_profile.profile.cooldown_until_ms = Some(0);
        repository
            .insert_profile(&cooldown_expired_profile)
            .unwrap();

        let status = repository
            .gateway_settings(false, false, Vec::new())
            .unwrap();

        assert_eq!(status.available_profiles, 3);
        assert_eq!(status.cooling_profiles, 2);
    }

    #[test]
    fn gateway_status_reports_a_healthy_direct_bridge_outside_the_pool() {
        let repository = Repository::memory();
        let mut oauth = stored_profile("oauth-login", ProfileKind::CodexOauth, "healthy");
        oauth.profile.in_pool = false;
        repository.insert_profile(&oauth).unwrap();
        let mut direct = stored_profile("zeron", ProfileKind::ApiKey, "healthy");
        direct.profile.in_pool = false;
        direct.profile.codex_oauth_profile_id = Some("oauth-login".to_owned());
        repository.insert_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "zeron")
            .unwrap();

        let status = repository.gateway_settings(true, true, Vec::new()).unwrap();

        assert_eq!(status.pool_status, "unavailable");
        assert_eq!(status.available_profiles, 0);
        let direct = status.direct_route.expect("direct route health");
        assert_eq!(direct.status, "ok");
        assert_eq!(direct.route_mode, "relay_bridge");
        assert!(direct.oauth_ready);
        assert!(direct.credential_ready);
    }

    #[test]
    fn desktop_workspace_mode_is_fixed_to_shared_and_rejects_unknown_values() {
        let repository = Repository::memory();
        assert_eq!(
            repository.desktop_workspace_mode().unwrap(),
            DesktopWorkspaceMode::Shared
        );
        for legacy_mode in [
            DesktopWorkspaceMode::Fresh,
            DesktopWorkspaceMode::PerProfile,
            DesktopWorkspaceMode::Shared,
        ] {
            repository.set_desktop_workspace_mode(&legacy_mode).unwrap();
            assert_eq!(
                repository.desktop_workspace_mode().unwrap(),
                DesktopWorkspaceMode::Shared
            );
        }
        repository
            .set_setting("desktop_workspace_mode", "fresh")
            .unwrap();
        assert_eq!(
            repository.desktop_workspace_mode().unwrap(),
            DesktopWorkspaceMode::Shared
        );
        repository
            .set_setting("desktop_workspace_mode", "per_profile")
            .unwrap();
        assert_eq!(
            repository.desktop_workspace_mode().unwrap(),
            DesktopWorkspaceMode::Shared
        );
        repository
            .set_setting("desktop_workspace_mode", "untrusted")
            .unwrap();
        assert!(repository.desktop_workspace_mode().is_err());
    }

    #[test]
    fn defaults_to_stable_auto_update_settings_and_rejects_unknown_values() {
        let repository = Repository::memory();
        assert_eq!(
            repository.app_update_settings().unwrap(),
            AppUpdateSettings {
                channel: AppUpdateChannel::Stable,
                auto_check: true,
            }
        );
        repository
            .set_app_update_settings(AppUpdateChannel::Beta, false)
            .unwrap();
        assert_eq!(
            repository.app_update_settings().unwrap(),
            AppUpdateSettings {
                channel: AppUpdateChannel::Beta,
                auto_check: false,
            }
        );
        repository
            .set_setting("app_update_channel", "nightly")
            .unwrap();
        assert!(repository.app_update_settings().is_err());
    }

    fn insert_test_client_key(repository: &Repository, id: &str, name: &str, secret_ref: &str) {
        let key = MaskedClientKey {
            id: id.to_owned(),
            name: name.to_owned(),
            masked_value: "crl_••••test".to_owned(),
            created_at_ms: 1,
            last_used_at_ms: None,
            revoked: false,
            managed_by: "user".to_owned(),
            can_revoke: true,
        };
        repository
            .insert_client_key(&key, "hash", secret_ref)
            .unwrap();
    }

    #[tokio::test]
    async fn records_touches_and_deletes_fresh_workspaces_without_deleting_profiles() {
        let repository = Repository::memory();
        profiles::create_oauth_profile(
            &repository,
            Arc::new(MemorySecretStore::new()),
            "profile-1".into(),
            "个人账号".into(),
            &profiles::CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: None,
                account_id: None,
                last_refresh_ms: 1,
            },
        )
        .await
        .unwrap();
        repository
            .create_desktop_workspace("workspace-1", "profile-1", 10)
            .unwrap();
        repository
            .touch_desktop_workspace("workspace-1", 20)
            .unwrap();

        let workspaces = repository.list_desktop_workspaces().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].profile_alias, "个人账号");
        assert_eq!(workspaces[0].last_launched_at_ms, 20);

        repository.delete_desktop_workspace("workspace-1").unwrap();
        assert!(repository.list_desktop_workspaces().unwrap().is_empty());
        assert!(repository.profile("profile-1").is_ok());
    }

    #[test]
    fn local_secret_store_upgrade_marks_existing_credentials_for_reentry_once() {
        let repository = Repository::memory();
        let configured = stored_profile("legacy-oauth", ProfileKind::CodexOauth, "healthy");
        repository.insert_profile(&configured).unwrap();
        let mut missing = stored_profile("missing-oauth", ProfileKind::CodexOauth, "healthy");
        missing.profile.credential_configured = false;
        repository.insert_profile(&missing).unwrap();

        assert!(repository.ensure_local_secret_store_backend().unwrap());
        assert_eq!(
            repository
                .setting("secret_store_backend")
                .unwrap()
                .as_deref(),
            Some("local_vault_v1")
        );
        let profiles = repository.list_profiles().unwrap();
        let configured = profiles
            .iter()
            .find(|profile| profile.profile.id == "legacy-oauth")
            .unwrap();
        assert!(!configured.profile.credential_configured);
        assert_eq!(configured.profile.health, "reauthorization_required");
        let missing = profiles
            .iter()
            .find(|profile| profile.profile.id == "missing-oauth")
            .unwrap();
        assert!(!missing.profile.credential_configured);

        assert!(!repository.ensure_local_secret_store_backend().unwrap());
    }
}
