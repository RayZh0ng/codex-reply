use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    domain::{
        CodexAuthMode, CodexSessionEvent, CodexSessionSummary, CollaborationProjectBinding,
        CollaborationProvider, CollaborationSummary, DesktopWorkspaceHistoryItem,
        DesktopWorkspaceMode, FeishuProjectBinding, GatewayNetworkAddress, GatewayProvider,
        GatewayStatus, MaskedClientKey, MaskedCollaborationBot, MaskedFeishuBot, MaskedProfile,
        MetricsSnapshot, ProfileAccountSummary, ProfileKind, ProfileQuota, ProfileSubscription,
        GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
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
                      enabled INTEGER NOT NULL,
                      in_pool INTEGER NOT NULL,
                      priority INTEGER NOT NULL,
                      weight INTEGER NOT NULL,
                      models_json TEXT NOT NULL,
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
                      account_subscription_json TEXT
                    );
                    CREATE TABLE IF NOT EXISTS app_settings (
                      key TEXT PRIMARY KEY,
                      value TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS desktop_workspaces (
                      id TEXT PRIMARY KEY,
                      profile_id TEXT NOT NULL,
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
                      profile_id TEXT NOT NULL,
                      chat_id TEXT,
                      bind_code TEXT NOT NULL UNIQUE,
                      enabled INTEGER NOT NULL,
                      concurrency_limit INTEGER NOT NULL,
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
                    CREATE TABLE IF NOT EXISTS codex_sessions (
                      id TEXT PRIMARY KEY,
                      binding_id TEXT NOT NULL,
                      profile_id TEXT NOT NULL,
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
                      working_directory TEXT NOT NULL,
                      FOREIGN KEY(binding_id) REFERENCES collaboration_project_bindings(id) ON DELETE CASCADE,
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
            let session_columns = table_columns(connection, "codex_sessions")?;
            for (name, definition) in [
                ("provider", "TEXT NOT NULL DEFAULT 'feishu'"),
                ("provider_bot_id", "TEXT"),
                ("provider_chat_id", "TEXT"),
                ("provider_message_id", "TEXT"),
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
            migrate_feishu_to_collaboration(connection)?;
            Ok(())
        })
    }

    pub fn insert_profile(&self, stored: &StoredProfile) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO profiles(id, alias, kind, base_url, provider, enabled, in_pool, priority, weight, models_json, health, cooldown_until_ms, secret_ref, credential_configured, auth_mode, credential_fingerprint, account_display_name, account_email, account_id, account_updated_at_ms, account_quota_json, account_subscription_json)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
                    params![
                        stored.profile.id,
                        stored.profile.alias,
                        profile_kind_name(&stored.profile.kind),
                        stored.profile.base_url,
                        provider_name(&stored.profile.provider),
                        stored.profile.enabled,
                        stored.profile.in_pool,
                        stored.profile.priority,
                        stored.profile.weight,
                        serde_json::to_string(&stored.profile.models).map_err(|_| AppError::Internal)?,
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
                .prepare("SELECT id, alias, kind, base_url, provider, enabled, in_pool, priority, weight, models_json, health, cooldown_until_ms, secret_ref, credential_configured, auth_mode, credential_fingerprint, account_display_name, account_email, account_id, account_updated_at_ms, account_quota_json, account_subscription_json FROM profiles ORDER BY priority, alias")
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let kind: String = row.get(2)?;
                    let models_json: String = row.get(9)?;
                    let models = serde_json::from_str(&models_json).unwrap_or_default();
                    Ok(StoredProfile {
                        secret_ref: row.get(12)?,
                        credential_fingerprint: row.get(15)?,
                        profile: MaskedProfile {
                            id: id.clone(),
                            alias: row.get(1)?,
                            kind: parse_profile_kind(&kind).unwrap_or(ProfileKind::ApiKey),
                            base_url: row.get(3)?,
                            provider: parse_provider(&row.get::<_, String>(4)?).unwrap_or_default(),
                            enabled: row.get(5)?,
                            in_pool: row.get(6)?,
                            priority: row.get(7)?,
                            weight: row.get(8)?,
                            models,
                            health: row.get(10)?,
                            cooldown_until_ms: row.get(11)?,
                            credential_configured: row.get(13)?,
                            auth_mode: parse_auth_mode(&row.get::<_, String>(14)?)
                                .unwrap_or_default(),
                            is_current: current_id.as_deref() == Some(id.as_str()),
                            account: account_summary(
                                row.get(16)?,
                                row.get(17)?,
                                row.get(18)?,
                                row.get(19)?,
                                row.get(20)?,
                                row.get(21)?,
                            ),
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
                    "UPDATE profiles SET alias = ?2, provider = ?3, enabled = ?4, in_pool = ?5, priority = ?6, weight = ?7, models_json = ?8, secret_ref = ?9, credential_configured = ?10, auth_mode = ?11, credential_fingerprint = ?12, account_display_name = ?13, account_email = ?14, account_id = ?15, account_updated_at_ms = ?16, account_quota_json = ?17, account_subscription_json = ?18 WHERE id = ?1",
                    params![
                        stored.profile.id,
                        stored.profile.alias,
                        provider_name(&stored.profile.provider),
                        stored.profile.enabled,
                        stored.profile.in_pool,
                        stored.profile.priority,
                        stored.profile.weight,
                        serde_json::to_string(&stored.profile.models).map_err(|_| AppError::Internal)?,
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
            None | Some("per_profile") => Ok(DesktopWorkspaceMode::PerProfile),
            Some("fresh") => Ok(DesktopWorkspaceMode::Fresh),
            Some("shared") => Ok(DesktopWorkspaceMode::Shared),
            Some(_) => Err(AppError::ValidationFailed),
        }
    }

    pub fn set_desktop_workspace_mode(&self, mode: &DesktopWorkspaceMode) -> AppResult<()> {
        let value = match mode {
            DesktopWorkspaceMode::Fresh => "fresh",
            DesktopWorkspaceMode::PerProfile => "per_profile",
            DesktopWorkspaceMode::Shared => "shared",
        };
        self.set_setting("desktop_workspace_mode", value)
    }

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
            Ok(GatewayStatus {
                running,
                bind_mode,
                bind_address: bind_address.clone(),
                available_addresses,
                port,
                cidrs: serde_json::from_str(&cidrs_json).unwrap_or_default(),
                available_profiles: profiles.iter().filter(|p| {
                    let profile = &p.profile;
                    (profile.kind == ProfileKind::ApiKey
                        || (profile.kind == ProfileKind::CodexOauth
                            && profile.auth_mode == CodexAuthMode::OAuth))
                        && profile.enabled
                        && profile.in_pool
                        && profile.credential_configured
                        && !matches!(
                            profile.health.as_str(),
                            "unhealthy" | "reauthorization_required"
                        )
                        && profile
                            .cooldown_until_ms
                            .is_none_or(|until| until <= now_ms)
                }).count(),
                cooling_profiles: profiles.iter().filter(|p| {
                    let profile = &p.profile;
                    (profile.kind == ProfileKind::ApiKey
                        || (profile.kind == ProfileKind::CodexOauth
                            && profile.auth_mode == CodexAuthMode::OAuth))
                        && profile.cooldown_until_ms.is_some()
                }).count(),
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

    pub fn revoke_client_key(&self, id: &str) -> AppResult<Option<String>> {
        self.with_connection(|connection| {
            let secret = connection
                .query_row(
                    "SELECT secret_ref FROM client_keys WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let Some(secret_ref) = secret else {
                return Err(AppError::NotFound);
            };
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
                        updated_at_ms: row.get(11)?,
                    },
                    config_json: row.get(6)?,
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
                "INSERT INTO collaboration_project_bindings(id, provider, bot_id, project_name, project_slug, working_directory, profile_id, chat_id, bind_code, enabled, concurrency_limit, created_at_ms, updated_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(id) DO UPDATE SET provider = excluded.provider, bot_id = excluded.bot_id, project_name = excluded.project_name, project_slug = excluded.project_slug, working_directory = excluded.working_directory, profile_id = excluded.profile_id, enabled = excluded.enabled, concurrency_limit = excluded.concurrency_limit, updated_at_ms = excluded.updated_at_ms",
                params![
                    binding.id,
                    collaboration_provider_name(&binding.provider),
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

    pub fn list_collaboration_project_bindings(
        &self,
    ) -> AppResult<Vec<CollaborationProjectBinding>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT b.id, b.provider, b.bot_id, bot.name, b.project_name, b.project_slug, b.working_directory, b.profile_id, p.alias, b.chat_id, b.bind_code, b.enabled, b.concurrency_limit, b.created_at_ms, b.updated_at_ms
                 FROM collaboration_project_bindings b
                 JOIN collaboration_bots bot ON bot.id = b.bot_id
                 JOIN profiles p ON p.id = b.profile_id
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
            ).map_err(|_| AppError::Internal)?;
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
            ).map_err(|_| AppError::Internal)?;
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
                .map_err(|_| AppError::Internal)?;
            connection.execute(
                "INSERT INTO codex_sessions(id, binding_id, profile_id, provider, provider_bot_id, provider_chat_id, provider_message_id, relay_status, codex_session_id, feishu_message_id, feishu_chat_id, started_by, started_at_ms, updated_at_ms, finished_at_ms, summary, last_error, working_directory)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    stored.session.id,
                    stored.session.binding_id,
                    stored.session.profile_id,
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
                    stored.working_directory,
                ],
            ).map_err(|_| AppError::Internal)?;
            connection
                .execute("PRAGMA foreign_keys = ON", [])
                .map_err(|_| AppError::Internal)?;
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
            ).map_err(|_| AppError::Internal)?;
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
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn set_codex_session_provider_message(&self, id: &str, message_id: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE codex_sessions SET provider_message_id = ?2, feishu_message_id = CASE WHEN provider = 'feishu' THEN ?2 ELSE feishu_message_id END, updated_at_ms = ?3 WHERE id = ?1",
                params![id, message_id, timestamp_ms()],
            ).map_err(|_| AppError::Internal)?;
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
                "SELECT s.id, s.binding_id, s.provider, s.provider_bot_id, s.provider_chat_id, s.provider_message_id, b.project_name, b.project_slug, s.profile_id, p.alias, s.relay_status, s.codex_session_id, s.feishu_message_id, s.feishu_chat_id, s.started_by, s.started_at_ms, s.updated_at_ms, s.finished_at_ms, s.summary, s.last_error, s.working_directory
                 FROM codex_sessions s JOIN collaboration_project_bindings b ON b.id = s.binding_id JOIN profiles p ON p.id = s.profile_id WHERE s.binding_id = ?1 ORDER BY s.started_at_ms DESC"
            } else {
                "SELECT s.id, s.binding_id, s.provider, s.provider_bot_id, s.provider_chat_id, s.provider_message_id, b.project_name, b.project_slug, s.profile_id, p.alias, s.relay_status, s.codex_session_id, s.feishu_message_id, s.feishu_chat_id, s.started_by, s.started_at_ms, s.updated_at_ms, s.finished_at_ms, s.summary, s.last_error, s.working_directory
                 FROM codex_sessions s JOIN collaboration_project_bindings b ON b.id = s.binding_id JOIN profiles p ON p.id = s.profile_id ORDER BY s.started_at_ms DESC LIMIT 200"
            };
            let mut statement = connection.prepare(sql).map_err(|_| AppError::Internal)?;
            let collect = |row: &rusqlite::Row<'_>| codex_session_from_row(row);
            if let Some(binding_id) = binding_id {
                let rows = statement.query_map(params![binding_id], collect).map_err(|_| AppError::Internal)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
            } else {
                let rows = statement.query_map([], collect).map_err(|_| AppError::Internal)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
            }
        })
    }

    pub fn codex_session(&self, id: &str) -> AppResult<StoredCodexSession> {
        self.with_connection(|connection| {
            connection.query_row(
                "SELECT s.id, s.binding_id, s.provider, s.provider_bot_id, s.provider_chat_id, s.provider_message_id, b.project_name, b.project_slug, s.profile_id, p.alias, s.relay_status, s.codex_session_id, s.feishu_message_id, s.feishu_chat_id, s.started_by, s.started_at_ms, s.updated_at_ms, s.finished_at_ms, s.summary, s.last_error, s.working_directory
                 FROM codex_sessions s JOIN collaboration_project_bindings b ON b.id = s.binding_id JOIN profiles p ON p.id = s.profile_id WHERE s.id = ?1",
                params![id],
                codex_session_from_row,
            ).map_err(|_| AppError::NotFound)
        })
    }

    pub fn count_running_codex_sessions(&self, binding_id: Option<&str>) -> AppResult<i64> {
        self.with_connection(|connection| {
            if let Some(binding_id) = binding_id {
                connection.query_row(
                    "SELECT COUNT(*) FROM codex_sessions WHERE binding_id = ?1 AND relay_status = 'running'",
                    params![binding_id],
                    |row| row.get(0),
                ).map_err(|_| AppError::Internal)
            } else {
                connection.query_row(
                    "SELECT COUNT(*) FROM codex_sessions WHERE relay_status = 'running'",
                    [],
                    |row| row.get(0),
                ).map_err(|_| AppError::Internal)
            }
        })
    }

    pub fn insert_codex_session_event(&self, event: &CodexSessionEvent) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO codex_session_events(id, session_id, occurred_at_ms, event_type, content) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![event.id, event.session_id, event.occurred_at_ms, event.event_type, event.content],
            ).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn metrics(&self) -> AppResult<MetricsSnapshot> {
        self.with_connection(|connection| {
            connection.query_row("SELECT total_requests, successful_requests, failed_requests, total_latency_ms, latency_samples, estimated_tokens FROM metrics WHERE id = 1", [], |row| {
                let samples: i64 = row.get(4)?;
                let latency: i64 = row.get(3)?;
                let average_latency_ms = if samples > 0 {
                    Some(latency / samples)
                } else {
                    None
                };
                Ok(MetricsSnapshot {
                    total_requests: row.get(0)?,
                    successful_requests: row.get(1)?,
                    failed_requests: row.get(2)?,
                    average_latency_ms,
                    estimated_tokens: row.get(5)?,
                })
            }).map_err(|_| AppError::Internal)
        })
    }

    pub fn record_metric(&self, successful: bool, latency_ms: i64) -> AppResult<()> {
        self.with_connection(|connection| { connection.execute("UPDATE metrics SET total_requests = total_requests + 1, successful_requests = successful_requests + ?1, failed_requests = failed_requests + ?2, total_latency_ms = total_latency_ms + ?3, latency_samples = latency_samples + 1 WHERE id = 1", params![i64::from(successful), i64::from(!successful), latency_ms]).map_err(|_| AppError::Internal)?; Ok(()) })
    }
}

#[allow(dead_code)]
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
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
    })
}

fn codex_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCodexSession> {
    let working_directory: String = row.get(20)?;
    let provider: String = row.get(2)?;
    Ok(StoredCodexSession {
        session: CodexSessionSummary {
            id: row.get(0)?,
            binding_id: row.get(1)?,
            provider: parse_collaboration_provider(&provider)
                .unwrap_or(CollaborationProvider::Feishu),
            provider_bot_id: row.get(3)?,
            provider_chat_id: row.get(4)?,
            provider_message_id: row.get(5)?,
            project_name: row.get(6)?,
            project_slug: row.get(7)?,
            profile_id: row.get(8)?,
            profile_alias: row.get(9)?,
            relay_status: row.get(10)?,
            codex_session_id: row.get(11)?,
            feishu_message_id: row.get(12)?,
            feishu_chat_id: row.get(13)?,
            started_by: row.get(14)?,
            started_at_ms: row.get(15)?,
            updated_at_ms: row.get(16)?,
            finished_at_ms: row.get(17)?,
            summary: row.get(18)?,
            last_error: row.get(19)?,
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

fn parse_provider(value: &str) -> Option<GatewayProvider> {
    match value {
        "openai" => Some(GatewayProvider::OpenAi),
        "openai_compatible" => Some(GatewayProvider::OpenAiCompatible),
        "anthropic" => Some(GatewayProvider::Anthropic),
        "gemini" => Some(GatewayProvider::Gemini),
        "ollama" => Some(GatewayProvider::Ollama),
        _ => None,
    }
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
    use super::{profile_columns, Repository, StoredProfile};
    use crate::{
        domain::{
            CollaborationProvider, CollaborationSummary, DesktopWorkspaceMode, GatewayProvider,
            MaskedClientKey, MaskedProfile, ProfileKind, GATEWAY_CODEX_CLIENT_KEY_REF_SETTING,
        },
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
                enabled: true,
                in_pool: true,
                priority: 0,
                weight: 1,
                models: vec!["gpt-5".into()],
                health: health.into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                is_current: false,
                account: None,
            },
            secret_ref: Some(format!("profile:{id}:credential")),
            credential_fingerprint: None,
        }
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
        let profile = repository.profile("legacy").unwrap().profile;
        assert!(profile.account.is_none());
        assert!(profile.credential_configured);
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
                  profile_id TEXT NOT NULL,
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
                enabled: true,
                in_pool: true,
                priority: 0,
                weight: 1,
                models: vec!["gpt-5".into()],
                health: health.into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                is_current: false,
                account: None,
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
    fn defaults_to_a_per_profile_workspace_and_rejects_unknown_values() {
        let repository = Repository::memory();
        assert_eq!(
            repository.desktop_workspace_mode().unwrap(),
            DesktopWorkspaceMode::PerProfile
        );
        repository
            .set_desktop_workspace_mode(&DesktopWorkspaceMode::Shared)
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
