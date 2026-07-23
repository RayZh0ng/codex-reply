use std::{path::Path, sync::Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    domain::{
        ChannelKind, DesktopWorkspaceHistoryItem, DesktopWorkspaceMode, GatewayStatus,
        MaskedChannel, MaskedClientKey, MaskedProfile, MetricsSnapshot, ProfileAccountSummary,
        ProfileKind, ProfileQuota, ProfileSubscription,
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
}

#[derive(Debug, Clone)]
pub struct StoredChannel {
    pub channel: MaskedChannel,
    pub endpoint_ref: String,
    pub signing_ref: Option<String>,
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
                      enabled INTEGER NOT NULL,
                      in_pool INTEGER NOT NULL,
                      priority INTEGER NOT NULL,
                      weight INTEGER NOT NULL,
                      models_json TEXT NOT NULL,
                      health TEXT NOT NULL,
                      cooldown_until_ms INTEGER,
                      secret_ref TEXT,
                      credential_configured INTEGER NOT NULL DEFAULT 0,
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
                    CREATE TABLE IF NOT EXISTS channels (
                      id TEXT PRIMARY KEY,
                      name TEXT NOT NULL,
                      kind TEXT NOT NULL,
                      enabled INTEGER NOT NULL,
                      endpoint_mask TEXT NOT NULL,
                      endpoint_ref TEXT NOT NULL,
                      signing_ref TEXT,
                      last_status TEXT NOT NULL
                    );
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
                ("account_display_name", "TEXT"),
                ("account_email", "TEXT"),
                ("account_id", "TEXT"),
                ("account_updated_at_ms", "INTEGER"),
                ("account_quota_json", "TEXT"),
                ("account_subscription_json", "TEXT"),
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
            Ok(())
        })
    }

    pub fn insert_profile(&self, stored: &StoredProfile) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO profiles(id, alias, kind, base_url, enabled, in_pool, priority, weight, models_json, health, cooldown_until_ms, secret_ref, credential_configured, account_display_name, account_email, account_id, account_updated_at_ms, account_quota_json, account_subscription_json)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                    params![
                        stored.profile.id,
                        stored.profile.alias,
                        profile_kind_name(&stored.profile.kind),
                        stored.profile.base_url,
                        stored.profile.enabled,
                        stored.profile.in_pool,
                        stored.profile.priority,
                        stored.profile.weight,
                        serde_json::to_string(&stored.profile.models).map_err(|_| AppError::Internal)?,
                        stored.profile.health,
                        stored.profile.cooldown_until_ms,
                        stored.secret_ref,
                        stored.profile.credential_configured,
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
                .prepare("SELECT id, alias, kind, base_url, enabled, in_pool, priority, weight, models_json, health, cooldown_until_ms, secret_ref, credential_configured, account_display_name, account_email, account_id, account_updated_at_ms, account_quota_json, account_subscription_json FROM profiles ORDER BY priority, alias")
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let kind: String = row.get(2)?;
                    let models_json: String = row.get(8)?;
                    let models = serde_json::from_str(&models_json).unwrap_or_default();
                    Ok(StoredProfile {
                        secret_ref: row.get(11)?,
                        profile: MaskedProfile {
                            id: id.clone(),
                            alias: row.get(1)?,
                            kind: parse_profile_kind(&kind).unwrap_or(ProfileKind::ApiKey),
                            base_url: row.get(3)?,
                            enabled: row.get(4)?,
                            in_pool: row.get(5)?,
                            priority: row.get(6)?,
                            weight: row.get(7)?,
                            models,
                            health: row.get(9)?,
                            cooldown_until_ms: row.get(10)?,
                            credential_configured: row.get(12)?,
                            is_current: current_id.as_deref() == Some(id.as_str()),
                            account: account_summary(
                                row.get(13)?,
                                row.get(14)?,
                                row.get(15)?,
                                row.get(16)?,
                                row.get(17)?,
                                row.get(18)?,
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
                    "UPDATE profiles SET alias = ?2, enabled = ?3, in_pool = ?4, priority = ?5, weight = ?6, models_json = ?7, secret_ref = ?8, credential_configured = ?9, account_display_name = ?10, account_email = ?11, account_id = ?12, account_updated_at_ms = ?13, account_quota_json = ?14, account_subscription_json = ?15 WHERE id = ?1",
                    params![
                        stored.profile.id,
                        stored.profile.alias,
                        stored.profile.enabled,
                        stored.profile.in_pool,
                        stored.profile.priority,
                        stored.profile.weight,
                        serde_json::to_string(&stored.profile.models).map_err(|_| AppError::Internal)?,
                        stored.secret_ref,
                        stored.profile.credential_configured,
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
    ) -> AppResult<GatewayStatus> {
        let profiles = self.list_profiles()?;
        self.with_connection(|connection| {
            let (bind_mode, bind_address, port, cidrs_json): (String, String, u16, String) = connection
                .query_row("SELECT bind_mode, bind_address, port, cidrs_json FROM gateway_settings WHERE id = 1", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .map_err(|_| AppError::Internal)?;
            let client_key_count = connection.query_row("SELECT COUNT(*) FROM client_keys WHERE revoked = 0", [], |row| row.get::<_, i64>(0)).map_err(|_| AppError::Internal)? as usize;
            Ok(GatewayStatus {
                running,
                bind_mode,
                bind_address,
                port,
                cidrs: serde_json::from_str(&cidrs_json).unwrap_or_default(),
                available_profiles: profiles.iter().filter(|p| p.profile.enabled && p.profile.in_pool && p.profile.health == "healthy").count(),
                cooling_profiles: profiles.iter().filter(|p| p.profile.cooldown_until_ms.is_some()).count(),
                client_key_count,
                certificate_ready,
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
            let mut statement = connection.prepare("SELECT id, name, masked_value, created_at_ms, last_used_at_ms, revoked FROM client_keys ORDER BY created_at_ms DESC").map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], |row| Ok(MaskedClientKey { id: row.get(0)?, name: row.get(1)?, masked_value: row.get(2)?, created_at_ms: row.get(3)?, last_used_at_ms: row.get(4)?, revoked: row.get(5)? })).map_err(|_| AppError::Internal)?;
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

    pub fn insert_channel(&self, stored: &StoredChannel) -> AppResult<()> {
        self.with_connection(|connection| {
            connection.execute("INSERT INTO channels(id, name, kind, enabled, endpoint_mask, endpoint_ref, signing_ref, last_status) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![stored.channel.id, stored.channel.name, channel_kind_name(&stored.channel.kind), stored.channel.enabled, stored.channel.endpoint_mask, stored.endpoint_ref, stored.signing_ref, stored.channel.last_status]).map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn update_channel(&self, stored: &StoredChannel) -> AppResult<()> {
        self.with_connection(|connection| {
            let changed = connection.execute("UPDATE channels SET name = ?2, kind = ?3, enabled = ?4, endpoint_mask = ?5, endpoint_ref = ?6, signing_ref = ?7 WHERE id = ?1", params![stored.channel.id, stored.channel.name, channel_kind_name(&stored.channel.kind), stored.channel.enabled, stored.channel.endpoint_mask, stored.endpoint_ref, stored.signing_ref]).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::NotFound); }
            Ok(())
        })
    }

    pub fn list_channels(&self) -> AppResult<Vec<StoredChannel>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare("SELECT id, name, kind, enabled, endpoint_mask, endpoint_ref, signing_ref, last_status FROM channels ORDER BY name").map_err(|_| AppError::Internal)?;
            let rows = statement.query_map([], |row| {
                let kind: String = row.get(2)?;
                Ok(StoredChannel { channel: MaskedChannel { id: row.get(0)?, name: row.get(1)?, kind: parse_channel_kind(&kind).unwrap_or(ChannelKind::Custom), enabled: row.get(3)?, endpoint_mask: row.get(4)?, last_status: row.get(7)? }, endpoint_ref: row.get(5)?, signing_ref: row.get(6)? })
            }).map_err(|_| AppError::Internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        })
    }

    pub fn channel(&self, id: &str) -> AppResult<StoredChannel> {
        self.list_channels()?
            .into_iter()
            .find(|item| item.channel.id == id)
            .ok_or(AppError::NotFound)
    }

    pub fn update_channel_status(&self, id: &str, status: &str) -> AppResult<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE channels SET last_status = ?2 WHERE id = ?1",
                    params![id, status],
                )
                .map_err(|_| AppError::Internal)?;
            Ok(())
        })
    }

    pub fn delete_channel(&self, id: &str) -> AppResult<(String, Option<String>)> {
        let stored = self.channel(id)?;
        self.with_connection(|connection| {
            connection
                .execute("DELETE FROM channels WHERE id = ?1", params![id])
                .map_err(|_| AppError::Internal)?;
            Ok((stored.endpoint_ref, stored.signing_ref))
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

fn profile_kind_name(kind: &ProfileKind) -> &'static str {
    match kind {
        ProfileKind::ApiKey => "api_key",
        ProfileKind::CodexOauth => "codex_oauth",
    }
}

fn profile_columns(connection: &Connection) -> AppResult<Vec<String>> {
    connection
        .prepare("PRAGMA table_info(profiles)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|_| AppError::Internal)
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
fn channel_kind_name(kind: &ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Feishu => "feishu",
        ChannelKind::Wecom => "wecom",
        ChannelKind::Custom => "custom",
    }
}
fn parse_channel_kind(value: &str) -> Option<ChannelKind> {
    match value {
        "feishu" => Some(ChannelKind::Feishu),
        "wecom" => Some(ChannelKind::Wecom),
        "custom" => Some(ChannelKind::Custom),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{profile_columns, Repository};
    use crate::{domain::DesktopWorkspaceMode, profiles, secrets::MemorySecretStore};
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

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
}
