use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use usage_core::{Account, AccountId, ProviderHealth, ProviderId, UsageSnapshot};

use crate::providers::{DailyUsageBucket, ProviderUsageEventBatch};

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
    pub health: Vec<ProviderHealth>,
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

pub(crate) struct CollectionRecord<'a> {
    pub snapshot: &'a UsageSnapshot,
    pub daily_usage: &'a [DailyUsageBucket],
    pub usage_events: Option<&'a ProviderUsageEventBatch>,
    pub health: &'a ProviderHealth,
    pub email: Option<&'a str>,
    pub backoff: Option<&'a StoredProviderBackoff>,
    pub clear_backoff: bool,
}

/// Read-only connections kept open alongside the single writer. WAL lets these
/// run concurrently with each other and with an in-flight write, so dashboard
/// reads (the hot path for GetState and GetUsage) never queue behind a
/// collection write.
const READ_POOL_SIZE: usize = 4;

#[derive(Clone)]
pub struct Storage {
    writer: Arc<ConnectionPool>,
    readers: Arc<ConnectionPool>,
}

impl Storage {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut writer = open_writer_connection(path)?;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
        migrations::migrate(&mut writer)?;

        let readers = (0..READ_POOL_SIZE)
            .map(|_| open_reader_connection(path))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            writer: Arc::new(ConnectionPool::new(
                path,
                ConnectionRole::Writer,
                vec![writer],
            )),
            readers: Arc::new(ConnectionPool::new(path, ConnectionRole::Reader, readers)),
        })
    }

    pub async fn provider_data_ids(&self) -> anyhow::Result<Vec<ProviderId>> {
        self.with_read_connection(|conn| {
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

    /// Runs a statement on the single writer connection. SQLite allows only one
    /// writer at a time, so funnelling every mutation through one connection
    /// keeps concurrent writes from colliding on the WAL.
    async fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        self.writer.execute(operation).await
    }

    /// Runs a read-only statement on the shared reader pool, concurrently with
    /// writes and other reads. Only pure `SELECT` operations may use this path.
    async fn with_read_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        self.readers.execute(operation).await
    }
}

/// A fixed set of interchangeable SQLite connections guarded by a semaphore
/// with one permit per connection. Because a permit is held for the whole
/// borrow, checkout and checkin only need a short `Vec` push/pop lock. A
/// poisoned bookkeeping lock is recovered instead of making a healthy SQLite
/// connection permanently unavailable.
struct ConnectionPool {
    path: std::path::PathBuf,
    role: ConnectionRole,
    idle: Mutex<Vec<Connection>>,
    permits: tokio::sync::Semaphore,
}

#[derive(Clone, Copy)]
enum ConnectionRole {
    Reader,
    Writer,
}

impl ConnectionPool {
    fn new(path: &Path, role: ConnectionRole, connections: Vec<Connection>) -> Self {
        Self {
            path: path.to_path_buf(),
            role,
            permits: tokio::sync::Semaphore::new(connections.len()),
            idle: Mutex::new(connections),
        }
    }

    async fn execute<T>(
        &self,
        operation: impl FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("sqlite connection pool closed"))?;
        // The permit guarantees an idle connection; reopen only in the unlikely
        // event a previous operation panicked and lost one.
        let connection = match self.checkout() {
            Some(connection) => connection,
            None => self.open_connection()?,
        };
        let handle = tokio::task::spawn_blocking(move || {
            let result = operation(&connection);
            (connection, result)
        });
        match handle.await {
            Ok((connection, result)) => {
                self.checkin(connection);
                result
            }
            Err(join_error) => {
                // The blocking closure panicked and dropped its connection.
                // Reopen one so the pool stays full for the next borrower.
                if let Ok(connection) = self.open_connection() {
                    self.checkin(connection);
                }
                Err(anyhow::anyhow!("sqlite operation panicked: {join_error}"))
            }
        }
    }

    fn checkout(&self) -> Option<Connection> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
    }

    fn checkin(&self, connection: Connection) {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(connection);
    }

    fn open_connection(&self) -> anyhow::Result<Connection> {
        match self.role {
            ConnectionRole::Reader => open_reader_connection(&self.path),
            ConnectionRole::Writer => open_writer_connection(&self.path),
        }
    }
}

fn open_writer_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "journal_size_limit", 4_i64 * 1024 * 1024)?;
    Ok(conn)
}

fn open_reader_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure_connection(&conn)?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

fn configure_connection(conn: &Connection) -> anyhow::Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -8_192_i64)?;
    Ok(())
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
