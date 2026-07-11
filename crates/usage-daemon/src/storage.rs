use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use usage_core::{Account, AccountId, ProviderId, UsageSnapshot};

mod accounts;
mod backoff;
mod health;
mod migrations;
mod notifications;
mod usage;

#[cfg(test)]
use usage::prune_account_history;
const SNAPSHOT_RETENTION_DAYS: u64 = 90;
const MAX_SNAPSHOTS_PER_ACCOUNT: usize = 10_000;
const MAX_RAW_PAYLOADS_PER_ACCOUNT: usize = 100;
const FORECAST_OBSERVATIONS_QUERY: &str = "SELECT collected_at, percent_used, reset_at
     FROM usage_window_observations
     WHERE provider_id = ?1
       AND account_id = ?2
       AND window_id = ?3
       AND collected_at >= ?4
       AND collected_at <= ?5
     ORDER BY collected_at DESC, snapshot_sequence DESC
     LIMIT ?6";
const UPSERT_DAILY_USAGE_QUERY: &str = "INSERT INTO provider_daily_usage
     (provider_id, account_id, usage_date, tokens, cost_usd, source, collected_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
     ON CONFLICT(provider_id, account_id, usage_date) DO UPDATE SET
       tokens = excluded.tokens,
       cost_usd = COALESCE(excluded.cost_usd, provider_daily_usage.cost_usd),
       source = excluded.source,
       collected_at = excluded.collected_at
     WHERE provider_daily_usage.tokens != excluded.tokens
        OR provider_daily_usage.cost_usd IS NOT
           COALESCE(excluded.cost_usd, provider_daily_usage.cost_usd)
        OR provider_daily_usage.source != excluded.source";

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum AccountIdentityConflict {
    #[error(
        "{provider_id} profile '{profile_id}' is already connected to external account \
         '{stored_external_account_id}' and cannot be changed to '{discovered_external_account_id}'"
    )]
    ProfileChanged {
        provider_id: String,
        profile_id: String,
        stored_external_account_id: String,
        discovered_external_account_id: String,
    },
    #[error(
        "{provider_id} external account '{external_account_id}' is already connected through \
         profile '{existing_profile_id}' and cannot also be connected through profile \
         '{discovered_profile_id}'"
    )]
    DuplicateExternalAccount {
        provider_id: String,
        external_account_id: String,
        existing_profile_id: String,
        discovered_profile_id: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredDailyUsage {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub date: chrono::NaiveDate,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredDailyUsageHistory {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub bucket_count: usize,
    pub total_tokens: u64,
    pub recent: Vec<StoredDailyUsage>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoredWindowObservation {
    pub collected_at: DateTime<Utc>,
    pub percent_used: f64,
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoredForecastHistory {
    pub by_window: HashMap<String, Vec<StoredWindowObservation>>,
}

#[derive(Clone, Debug)]
pub struct StoredUsageDashboard {
    pub snapshots: Vec<UsageSnapshot>,
    pub accounts: Vec<Account>,
    pub daily_usage: Vec<StoredDailyUsageHistory>,
    pub forecast_histories: HashMap<(ProviderId, AccountId), StoredForecastHistory>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NotificationWindowState {
    pub reset_at: Option<DateTime<Utc>>,
    pub notified_mask: u8,
    pub last_attempt_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredProviderBackoff {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub consecutive_failures: usize,
    pub retry_at: DateTime<Utc>,
    pub last_failure_at: DateTime<Utc>,
    pub error_message: String,
}

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    connection_gate: Arc<tokio::sync::Semaphore>,
}

impl Storage {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -8_192_i64)?;
        conn.pragma_update(None, "journal_size_limit", 4_i64 * 1024 * 1024)?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            connection_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
    pub async fn provider_data_ids(&self) -> anyhow::Result<Vec<ProviderId>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id FROM accounts
                 UNION
                 SELECT provider_id FROM usage_snapshots
                 ORDER BY provider_id",
            )?;
            let provider_ids = stmt
                .query_map([], |row| Ok(ProviderId::new(row.get::<_, String>(0)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(provider_ids)
        })
        .await
    }

    async fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        let permit = self
            .connection_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("sqlite connection gate closed"))?;
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("sqlite connection mutex poisoned"))?;
            operation(&conn)
        })
        .await?
    }
}

fn parse_time_sql(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })
}

fn parse_optional_time_sql(value: Option<String>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value.as_deref().map(parse_time_sql).transpose()
}

#[cfg(test)]
mod tests;
