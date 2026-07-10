use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use usage_core::{
    Account, AccountDisplayNameSource, AccountId, PendingNotification, ProviderHealth,
    ProviderHealthStatus, ProviderId, UsageSnapshot,
};
use uuid::Uuid;

use crate::providers::DailyUsageBucket;

const INITIAL_MIGRATION: &str = include_str!("../migrations/0001_initial.sql");
const DAILY_USAGE_MIGRATION: &str = include_str!("../migrations/0002_provider_daily_usage.sql");
const DAILY_USAGE_SUMMARY_MIGRATION: &str =
    include_str!("../migrations/0003_daily_usage_summary.sql");
const SNAPSHOT_RETENTION_INDEXES_MIGRATION: &str =
    include_str!("../migrations/0004_snapshot_retention_indexes.sql");
const ACCOUNT_IDENTITY_MIGRATION: &str = include_str!("../migrations/0005_account_identity.sql");
const NOTIFICATION_STATE_MIGRATION: &str =
    include_str!("../migrations/0006_notification_state.sql");
const PENDING_NOTIFICATIONS_MIGRATION: &str =
    include_str!("../migrations/0007_pending_notifications.sql");
const SNAPSHOT_RETENTION_DAYS: u64 = 90;
const MAX_SNAPSHOTS_PER_ACCOUNT: usize = 10_000;
const MAX_RAW_PAYLOADS_PER_ACCOUNT: usize = 100;

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

#[derive(Clone, Debug, PartialEq)]
pub struct NotificationWindowState {
    pub last_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub notified_mask: u8,
    pub last_attempt_at: Option<DateTime<Utc>>,
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
        let conn = Connection::open(path)?;
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
        conn.execute_batch(INITIAL_MIGRATION)?;
        migrate_account_profile_identity(&conn)?;
        migrate_account_lifecycle_state(&conn)?;
        migrate_account_identity(&conn)?;
        conn.execute_batch(DAILY_USAGE_MIGRATION)?;
        conn.execute_batch(DAILY_USAGE_SUMMARY_MIGRATION)?;
        conn.execute_batch(SNAPSHOT_RETENTION_INDEXES_MIGRATION)?;
        conn.execute_batch(NOTIFICATION_STATE_MIGRATION)?;
        conn.execute_batch(PENDING_NOTIFICATIONS_MIGRATION)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            connection_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    pub async fn upsert_account(
        &self,
        provider_id: &ProviderId,
        external_account_id: &str,
        profile_id: Option<&str>,
        display_name: Option<&str>,
        email: Option<&str>,
    ) -> anyhow::Result<Account> {
        let provider_id = provider_id.clone();
        let external_account_id = external_account_id.to_string();
        let profile_id = normalized_profile_id(profile_id, &external_account_id);
        let display_name = normalized_identity_value(display_name);
        let email = normalized_email(email).or_else(|| {
            display_name
                .as_deref()
                .filter(|value| looks_like_email(value))
                .map(ToOwned::to_owned)
        });
        let display_name = display_name.filter(|value| !looks_like_email(value));
        self.with_connection(move |conn| {
            let now = Utc::now();
            let existing = conn
                .query_row(
                    account_select_sql("WHERE provider_id = ?1 AND profile_id = ?2").as_str(),
                    params![provider_id.as_str(), profile_id.as_str()],
                    account_from_row,
                )
                .optional()?;

            let adopting_legacy_identity = existing.as_ref().is_some_and(|existing| {
                can_adopt_legacy_external_identity(
                    &provider_id,
                    &existing.external_account_id,
                    &external_account_id,
                )
            });
            if let Some(existing) = existing.as_ref() {
                if existing.external_account_id != external_account_id && !adopting_legacy_identity
                {
                    return Err(AccountIdentityConflict::ProfileChanged {
                        provider_id: provider_id.as_str().to_string(),
                        profile_id: profile_id.clone(),
                        stored_external_account_id: existing.external_account_id.clone(),
                        discovered_external_account_id: external_account_id.clone(),
                    }
                    .into());
                }
            }
            if provider_requires_unique_external_account(&provider_id)
                && (existing.is_none() || adopting_legacy_identity)
            {
                let existing_profile_id = conn
                    .query_row(
                        "SELECT profile_id FROM accounts
                         WHERE provider_id = ?1 AND external_account_id = ?2 AND profile_id != ?3
                         LIMIT 1",
                        params![
                            provider_id.as_str(),
                            external_account_id.as_str(),
                            profile_id.as_str()
                        ],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(existing_profile_id) = existing_profile_id {
                    return Err(AccountIdentityConflict::DuplicateExternalAccount {
                        provider_id: provider_id.as_str().to_string(),
                        external_account_id: external_account_id.clone(),
                        existing_profile_id,
                        discovered_profile_id: profile_id.clone(),
                    }
                    .into());
                }
            }

            let (
                id,
                created_at,
                hidden,
                collection_enabled,
                next_display_name,
                display_name_source,
                next_email,
            ) = if let Some(existing) = existing {
                let (next_display_name, display_name_source) =
                    if existing.display_name_source == AccountDisplayNameSource::User {
                        (existing.display_name, AccountDisplayNameSource::User)
                    } else if let Some(display_name) = display_name {
                        (Some(display_name), AccountDisplayNameSource::User)
                    } else {
                        (existing.display_name, existing.display_name_source)
                    };
                (
                    existing.id.to_string(),
                    existing.created_at,
                    existing.hidden,
                    existing.collection_enabled,
                    next_display_name,
                    display_name_source,
                    email.or(existing.email),
                )
            } else {
                let (next_display_name, display_name_source) = match display_name {
                    Some(display_name) => (Some(display_name), AccountDisplayNameSource::User),
                    None => (
                        Some(generated_account_display_name(conn, provider_id.as_str())?),
                        AccountDisplayNameSource::Generated,
                    ),
                };
                (
                    Uuid::new_v4().to_string(),
                    now,
                    false,
                    true,
                    next_display_name,
                    display_name_source,
                    email,
                )
            };
            conn.execute(
                "INSERT INTO accounts
             (id, provider_id, external_account_id, profile_id, display_name, display_name_source,
              email, hidden, collection_enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(provider_id, profile_id) DO UPDATE SET
               external_account_id = excluded.external_account_id,
               display_name = excluded.display_name,
               display_name_source = excluded.display_name_source,
               email = excluded.email,
               updated_at = excluded.updated_at",
                params![
                    id,
                    provider_id.as_str(),
                    external_account_id.as_str(),
                    profile_id.as_str(),
                    next_display_name.as_deref(),
                    display_name_source_sql(display_name_source),
                    next_email.as_deref(),
                    i64::from(hidden),
                    i64::from(collection_enabled),
                    created_at.to_rfc3339(),
                    now.to_rfc3339(),
                ],
            )?;

            Ok(Account {
                id: AccountId::new(id),
                provider_id,
                external_account_id,
                profile_id: (!profile_id.is_empty()).then_some(profile_id),
                display_name: next_display_name,
                display_name_source,
                email: next_email,
                hidden,
                collection_enabled,
                created_at,
                updated_at: now,
            })
        })
        .await
    }

    #[cfg(test)]
    pub async fn insert_snapshot(
        &self,
        snapshot: &UsageSnapshot,
        raw_payload: Option<&serde_json::Value>,
    ) -> anyhow::Result<()> {
        let snapshot = snapshot.clone();
        let raw_payload = raw_payload.cloned();
        self.with_connection(move |conn| {
            let snapshot_id = Uuid::new_v4().to_string();
            let normalized_json = serde_json::to_string(&snapshot)?;
            let metadata_json = serde_json::to_string(&snapshot.metadata)?;
            conn.execute(
                "INSERT INTO usage_snapshots
             (id, provider_id, account_id, collected_at, normalized_json, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    snapshot_id,
                    snapshot.provider_id.as_str(),
                    snapshot.account_id.as_str(),
                    snapshot.collected_at.to_rfc3339(),
                    normalized_json,
                    metadata_json,
                ],
            )?;

            if let Some(raw_payload) = raw_payload {
                conn.execute(
                    "INSERT INTO raw_payloads
                 (id, snapshot_id, provider_id, collected_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        Uuid::new_v4().to_string(),
                        snapshot_id,
                        snapshot.provider_id.as_str(),
                        snapshot.collected_at.to_rfc3339(),
                        serde_json::to_string(&raw_payload)?,
                    ],
                )?;
            }

            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub async fn upsert_daily_usage(
        &self,
        provider_id: &ProviderId,
        account_id: &AccountId,
        buckets: &[DailyUsageBucket],
        collected_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let provider_id = provider_id.clone();
        let account_id = account_id.clone();
        let buckets = buckets.to_vec();
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            for bucket in buckets {
                let tokens = i64::try_from(bucket.tokens).map_err(|_| {
                    anyhow::anyhow!(
                        "daily usage tokens exceed SQLite integer range for {}",
                        bucket.date
                    )
                })?;
                transaction.execute(
                    "INSERT INTO provider_daily_usage
                     (provider_id, account_id, usage_date, tokens, cost_usd, source, collected_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(provider_id, account_id, usage_date) DO UPDATE SET
                       tokens = excluded.tokens,
                       cost_usd = COALESCE(excluded.cost_usd, provider_daily_usage.cost_usd),
                       source = excluded.source,
                       collected_at = excluded.collected_at",
                    params![
                        provider_id.as_str(),
                        account_id.as_str(),
                        bucket.date.to_string(),
                        tokens,
                        bucket.cost_usd,
                        bucket.source,
                        collected_at.to_rfc3339(),
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub async fn daily_usage_history(&self) -> anyhow::Result<Vec<StoredDailyUsage>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, account_id, usage_date, tokens, cost_usd, source
                 FROM provider_daily_usage
                 ORDER BY provider_id, account_id, usage_date",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    let date: String = row.get(2)?;
                    let tokens: i64 = row.get(3)?;
                    Ok(StoredDailyUsage {
                        provider_id: ProviderId::new(row.get::<_, String>(0)?),
                        account_id: AccountId::new(row.get::<_, String>(1)?),
                        date: chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").map_err(
                            |err| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    rusqlite::types::Type::Text,
                                    Box::new(err),
                                )
                            },
                        )?,
                        tokens: u64::try_from(tokens).map_err(|err| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Integer,
                                Box::new(err),
                            )
                        })?,
                        cost_usd: row.get(4)?,
                        source: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn daily_usage_dashboard(
        &self,
        recent_since: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<StoredDailyUsageHistory>> {
        self.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT totals.provider_id, totals.account_id, totals.bucket_count,
                        totals.total_tokens, recent.usage_date, recent.tokens,
                        recent.cost_usd, recent.source
                 FROM provider_daily_usage_summary AS totals
                 LEFT JOIN provider_daily_usage AS recent
                   ON recent.provider_id = totals.provider_id
                  AND recent.account_id = totals.account_id
                  AND recent.usage_date >= ?1
                 ORDER BY totals.provider_id, totals.account_id, recent.usage_date",
            )?;
            let rows = stmt.query_map(params![recent_since.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<f64>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?;

            let mut histories = Vec::<StoredDailyUsageHistory>::new();
            for row in rows {
                let (
                    provider_id,
                    account_id,
                    bucket_count,
                    total_tokens,
                    date,
                    tokens,
                    cost_usd,
                    source,
                ) = row?;
                let is_new_history = histories.last().is_none_or(|history| {
                    history.provider_id.as_str() != provider_id
                        || history.account_id.as_str() != account_id
                });
                if is_new_history {
                    histories.push(StoredDailyUsageHistory {
                        provider_id: ProviderId::new(provider_id.clone()),
                        account_id: AccountId::new(account_id.clone()),
                        bucket_count: usize::try_from(bucket_count).map_err(|err| {
                            anyhow::anyhow!("daily usage bucket count was invalid: {err}")
                        })?,
                        total_tokens: u64::try_from(total_tokens).map_err(|err| {
                            anyhow::anyhow!("daily usage total was invalid: {err}")
                        })?,
                        recent: Vec::new(),
                    });
                }

                if let (Some(date), Some(tokens), Some(source)) = (date, tokens, source) {
                    let history = histories.last_mut().ok_or_else(|| {
                        anyhow::anyhow!("daily usage history was not initialized")
                    })?;
                    history.recent.push(StoredDailyUsage {
                        provider_id: ProviderId::new(provider_id),
                        account_id: AccountId::new(account_id),
                        date: chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")?,
                        tokens: u64::try_from(tokens).map_err(|err| {
                            anyhow::anyhow!("daily usage tokens were invalid: {err}")
                        })?,
                        cost_usd,
                        source,
                    });
                }
            }
            Ok(histories)
        })
        .await
    }

    pub async fn record_success(
        &self,
        snapshot: &UsageSnapshot,
        raw_payload: Option<&serde_json::Value>,
        daily_usage: &[DailyUsageBucket],
        health: &ProviderHealth,
        email: Option<&str>,
    ) -> anyhow::Result<()> {
        let snapshot = snapshot.clone();
        let raw_payload = raw_payload.cloned();
        let daily_usage = daily_usage.to_vec();
        let health = health.clone();
        let email = email
            .map(str::trim)
            .filter(|value| looks_like_email(value))
            .map(ToOwned::to_owned);
        self.with_connection(move |conn| {
            let snapshot_id = Uuid::new_v4().to_string();
            let normalized_json = serde_json::to_string(&snapshot)?;
            let metadata_json = serde_json::to_string(&snapshot.metadata)?;
            let raw_payload_json = raw_payload
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let collected_at = snapshot.collected_at.to_rfc3339();
            let transaction = conn.unchecked_transaction()?;

            for bucket in daily_usage {
                let tokens = i64::try_from(bucket.tokens).map_err(|_| {
                    anyhow::anyhow!(
                        "daily usage tokens exceed SQLite integer range for {}",
                        bucket.date
                    )
                })?;
                transaction.execute(
                    "INSERT INTO provider_daily_usage
                     (provider_id, account_id, usage_date, tokens, cost_usd, source, collected_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(provider_id, account_id, usage_date) DO UPDATE SET
                       tokens = excluded.tokens,
                       cost_usd = COALESCE(excluded.cost_usd, provider_daily_usage.cost_usd),
                       source = excluded.source,
                       collected_at = excluded.collected_at",
                    params![
                        snapshot.provider_id.as_str(),
                        snapshot.account_id.as_str(),
                        bucket.date.to_string(),
                        tokens,
                        bucket.cost_usd,
                        bucket.source,
                        collected_at,
                    ],
                )?;
            }

            transaction.execute(
                "INSERT INTO usage_snapshots
                 (id, provider_id, account_id, collected_at, normalized_json, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    snapshot_id,
                    snapshot.provider_id.as_str(),
                    snapshot.account_id.as_str(),
                    collected_at,
                    normalized_json,
                    metadata_json,
                ],
            )?;
            if let Some(payload_json) = raw_payload_json {
                transaction.execute(
                    "INSERT INTO raw_payloads
                     (id, snapshot_id, provider_id, collected_at, payload_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        Uuid::new_v4().to_string(),
                        snapshot_id,
                        snapshot.provider_id.as_str(),
                        collected_at,
                        payload_json,
                    ],
                )?;
            }

            let health_account_id = health
                .account_id
                .as_ref()
                .map(AccountId::as_str)
                .unwrap_or("");
            transaction.execute(
                "INSERT INTO provider_health
                 (provider_id, account_id, status, collection_mode, last_success_at, last_failure_at,
                  last_error_code, last_error_message, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(provider_id, account_id) DO UPDATE SET
                   status = excluded.status,
                   collection_mode = COALESCE(excluded.collection_mode, provider_health.collection_mode),
                   last_success_at = COALESCE(excluded.last_success_at, provider_health.last_success_at),
                   last_failure_at = COALESCE(excluded.last_failure_at, provider_health.last_failure_at),
                   last_error_code = excluded.last_error_code,
                   last_error_message = excluded.last_error_message,
                   updated_at = excluded.updated_at",
                params![
                    health.provider_id.as_str(),
                    health_account_id,
                    health_status_to_sql(&health.status),
                    health.collection_mode.as_deref(),
                    health.last_success_at.map(|time| time.to_rfc3339()),
                    health.last_failure_at.map(|time| time.to_rfc3339()),
                    health.last_error_code.as_deref(),
                    health.last_error_message.as_deref(),
                    health.updated_at.to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "DELETE FROM provider_health WHERE provider_id = ?1 AND account_id = ''",
                params![snapshot.provider_id.as_str()],
            )?;
            if let Some(email) = email {
                transaction.execute(
                    "UPDATE accounts SET email = ?1, updated_at = ?2 WHERE id = ?3",
                    params![
                        email,
                        Utc::now().to_rfc3339(),
                        snapshot.account_id.as_str()
                    ],
                )?;
            }
            let retention_cutoff = Utc::now()
                .checked_sub_days(chrono::Days::new(SNAPSHOT_RETENTION_DAYS))
                .unwrap_or_else(Utc::now);
            prune_account_history(
                &transaction,
                &snapshot.provider_id,
                &snapshot.account_id,
                retention_cutoff,
                MAX_SNAPSHOTS_PER_ACCOUNT,
                MAX_RAW_PAYLOADS_PER_ACCOUNT,
            )?;

            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn account(&self, account_id: &AccountId) -> anyhow::Result<Option<Account>> {
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            conn.query_row(
                account_select_sql("WHERE id = ?1").as_str(),
                params![account_id.as_str()],
                account_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await
    }

    pub async fn update_account(
        &self,
        account_id: &AccountId,
        display_name: Option<&str>,
        hidden: Option<bool>,
        collection_enabled: Option<bool>,
    ) -> anyhow::Result<Account> {
        let account_id = account_id.clone();
        let display_name = display_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        self.with_connection(move |conn| {
            let existing = conn
                .query_row(
                    account_select_sql("WHERE id = ?1").as_str(),
                    params![account_id.as_str()],
                    account_from_row,
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
            let next_display_name = display_name.as_deref().or(existing.display_name.as_deref());
            let next_display_name_source = if display_name.is_some() {
                AccountDisplayNameSource::User
            } else {
                existing.display_name_source
            };
            let next_hidden = hidden.unwrap_or(existing.hidden);
            let next_collection_enabled = collection_enabled.unwrap_or(existing.collection_enabled);
            let updated_at = Utc::now();
            conn.execute(
                "UPDATE accounts
                 SET display_name = ?1,
                     display_name_source = ?2,
                     hidden = ?3,
                     collection_enabled = ?4,
                     updated_at = ?5
                 WHERE id = ?6",
                params![
                    next_display_name,
                    display_name_source_sql(next_display_name_source),
                    i64::from(next_hidden),
                    i64::from(next_collection_enabled),
                    updated_at.to_rfc3339(),
                    account_id.as_str(),
                ],
            )?;
            Ok(Account {
                display_name: next_display_name.map(ToOwned::to_owned),
                display_name_source: next_display_name_source,
                hidden: next_hidden,
                collection_enabled: next_collection_enabled,
                updated_at,
                ..existing
            })
        })
        .await
    }

    pub async fn delete_account(&self, account_id: &AccountId) -> anyhow::Result<()> {
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            transaction.execute(
                "DELETE FROM raw_payloads
                 WHERE snapshot_id IN (SELECT id FROM usage_snapshots WHERE account_id = ?1)",
                params![account_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM usage_snapshots WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM provider_health WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM provider_daily_usage WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
            let deleted = transaction.execute(
                "DELETE FROM accounts WHERE id = ?1",
                params![account_id.as_str()],
            )?;
            if deleted == 0 {
                anyhow::bail!("unknown account: {}", account_id.as_str());
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn notification_window_state(
        &self,
        account_id: &AccountId,
        window_id: &str,
    ) -> anyhow::Result<Option<NotificationWindowState>> {
        let account_id = account_id.clone();
        let window_id = window_id.to_string();
        self.with_connection(move |conn| {
            let row = conn
                .query_row(
                    "SELECT last_percent, reset_at, notified_mask, last_attempt_at
                     FROM notification_window_state
                     WHERE account_id = ?1 AND window_id = ?2",
                    params![account_id.as_str(), window_id],
                    |row| {
                        let reset_at: Option<String> = row.get(1)?;
                        let last_attempt_at: Option<String> = row.get(3)?;
                        let notified_mask: i64 = row.get(2)?;
                        Ok((
                            row.get::<_, f64>(0)?,
                            reset_at,
                            notified_mask,
                            last_attempt_at,
                        ))
                    },
                )
                .optional()?;
            row.map(|(last_percent, reset_at, notified_mask, last_attempt_at)| {
                Ok(NotificationWindowState {
                    last_percent,
                    reset_at: reset_at.as_deref().map(parse_time_sql).transpose()?,
                    notified_mask: u8::try_from(notified_mask)?,
                    last_attempt_at: last_attempt_at.as_deref().map(parse_time_sql).transpose()?,
                })
            })
            .transpose()
        })
        .await
    }

    pub async fn upsert_notification_window_state(
        &self,
        account_id: &AccountId,
        window_id: &str,
        state: NotificationWindowState,
    ) -> anyhow::Result<()> {
        let account_id = account_id.clone();
        let window_id = window_id.to_string();
        self.with_connection(move |conn| {
            conn.execute(
                "INSERT INTO notification_window_state
                 (account_id, window_id, last_percent, reset_at, notified_mask, last_attempt_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(account_id, window_id) DO UPDATE SET
                   last_percent = excluded.last_percent,
                   reset_at = excluded.reset_at,
                   notified_mask = excluded.notified_mask,
                   last_attempt_at = excluded.last_attempt_at",
                params![
                    account_id.as_str(),
                    window_id,
                    state.last_percent,
                    state.reset_at.map(|value| value.to_rfc3339()),
                    i64::from(state.notified_mask),
                    state.last_attempt_at.map(|value| value.to_rfc3339()),
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn clear_notification_window_state(&self) -> anyhow::Result<()> {
        self.with_connection(|conn| {
            conn.execute("DELETE FROM notification_window_state", [])?;
            Ok(())
        })
        .await
    }

    pub async fn enqueue_notification(&self, title: &str, body: &str) -> anyhow::Result<()> {
        let title = title.to_string();
        let body = body.to_string();
        self.with_connection(move |conn| {
            conn.execute(
                "INSERT INTO pending_notifications (title, body, created_at) VALUES (?1, ?2, ?3)",
                params![title, body, Utc::now().to_rfc3339()],
            )?;
            conn.execute(
                "DELETE FROM pending_notifications WHERE id NOT IN
                 (SELECT id FROM pending_notifications ORDER BY id DESC LIMIT 1000)",
                [],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn pending_notifications(&self) -> anyhow::Result<Vec<PendingNotification>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, body, created_at FROM pending_notifications
                 ORDER BY id ASC LIMIT 100",
            )?;
            let notifications = stmt
                .query_map([], |row| {
                    let created_at: String = row.get(3)?;
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        created_at,
                    ))
                })?
                .map(|row| {
                    let (id, title, body, created_at) = row?;
                    Ok(PendingNotification {
                        id,
                        title,
                        body,
                        created_at: parse_time_sql(&created_at)?,
                    })
                })
                .collect();
            notifications
        })
        .await
    }

    pub async fn acknowledge_notifications(&self, ids: &[i64]) -> anyhow::Result<()> {
        let ids = ids.to_vec();
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            for id in ids {
                transaction.execute("DELETE FROM pending_notifications WHERE id = ?1", [id])?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn clear_pending_notifications(&self) -> anyhow::Result<()> {
        self.with_connection(|conn| {
            conn.execute("DELETE FROM pending_notifications", [])?;
            Ok(())
        })
        .await
    }

    pub async fn upsert_health(&self, health: &ProviderHealth) -> anyhow::Result<()> {
        let health = health.clone();
        self.with_connection(move |conn| {
            let account_id = health
                .account_id
                .as_ref()
                .map(AccountId::as_str)
                .unwrap_or("");
            conn.execute(
                "INSERT INTO provider_health
                 (provider_id, account_id, status, collection_mode, last_success_at, last_failure_at,
                  last_error_code, last_error_message, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(provider_id, account_id) DO UPDATE SET
                   status = excluded.status,
                   collection_mode = COALESCE(excluded.collection_mode, provider_health.collection_mode),
                   last_success_at = COALESCE(excluded.last_success_at, provider_health.last_success_at),
                   last_failure_at = COALESCE(excluded.last_failure_at, provider_health.last_failure_at),
                   last_error_code = excluded.last_error_code,
                   last_error_message = excluded.last_error_message,
                   updated_at = excluded.updated_at",
                params![
                    health.provider_id.as_str(),
                    account_id,
                    health_status_to_sql(&health.status),
                    health.collection_mode.as_deref(),
                    health.last_success_at.map(|time| time.to_rfc3339()),
                    health.last_failure_at.map(|time| time.to_rfc3339()),
                    health.last_error_code.as_deref(),
                    health.last_error_message.as_deref(),
                    health.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub async fn delete_provider_level_health(
        &self,
        provider_id: &ProviderId,
    ) -> anyhow::Result<()> {
        let provider_id = provider_id.clone();
        self.with_connection(move |conn| {
            conn.execute(
                "DELETE FROM provider_health WHERE provider_id = ?1 AND account_id = ''",
                params![provider_id.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn latest_usage(&self) -> anyhow::Result<Vec<UsageSnapshot>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT latest.normalized_json
             FROM accounts AS account
             JOIN usage_snapshots AS latest ON latest.rowid = (
               SELECT rowid FROM usage_snapshots
               WHERE provider_id = account.provider_id AND account_id = account.id
               ORDER BY collected_at DESC, rowid DESC
               LIMIT 1
             )
             ORDER BY account.provider_id, account.id",
            )?;
            let snapshots = stmt
                .query_map([], usage_snapshot_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(snapshots)
        })
        .await
    }

    pub async fn recent_usage(
        &self,
        provider_id: &ProviderId,
        account_id: &AccountId,
        since: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<Vec<UsageSnapshot>> {
        let provider_id = provider_id.clone();
        let account_id = account_id.clone();
        let limit = limit.min(MAX_SNAPSHOTS_PER_ACCOUNT) as i64;
        self.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT normalized_json FROM usage_snapshots
                 WHERE provider_id = ?1 AND account_id = ?2 AND collected_at >= ?3
                 ORDER BY collected_at DESC, rowid DESC
                 LIMIT ?4",
            )?;
            let snapshots = stmt
                .query_map(
                    params![
                        provider_id.as_str(),
                        account_id.as_str(),
                        since.to_rfc3339(),
                        limit,
                    ],
                    usage_snapshot_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(snapshots)
        })
        .await
    }

    pub async fn accounts(&self) -> anyhow::Result<Vec<Account>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                account_select_sql("ORDER BY provider_id, profile_id, external_account_id")
                    .as_str(),
            )?;
            let accounts = stmt
                .query_map([], account_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(accounts)
        })
        .await
    }

    pub async fn provider_health(&self) -> anyhow::Result<Vec<ProviderHealth>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, account_id, status, collection_mode, last_success_at,
                    last_failure_at, last_error_code, last_error_message, updated_at
             FROM provider_health
             ORDER BY provider_id, account_id",
            )?;
            let health = stmt
                .query_map([], provider_health_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(health)
        })
        .await
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

fn prune_account_history(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: &AccountId,
    retention_cutoff: DateTime<Utc>,
    max_snapshots: usize,
    max_raw_payloads: usize,
) -> anyhow::Result<()> {
    let max_snapshots = i64::try_from(max_snapshots)?;
    let max_raw_payloads = i64::try_from(max_raw_payloads)?;
    conn.execute(
        "DELETE FROM raw_payloads
         WHERE id IN (
           SELECT raw.id
           FROM raw_payloads AS raw
           JOIN usage_snapshots AS snapshot ON snapshot.id = raw.snapshot_id
           WHERE snapshot.provider_id = ?1 AND snapshot.account_id = ?2
           ORDER BY raw.collected_at DESC, raw.rowid DESC
           LIMIT -1 OFFSET ?3
         )",
        params![provider_id.as_str(), account_id.as_str(), max_raw_payloads],
    )?;
    let old_snapshot_ids = "SELECT id FROM usage_snapshots
         WHERE provider_id = ?1 AND account_id = ?2
           AND (
             collected_at < ?3
             OR id IN (
               SELECT id FROM usage_snapshots
               WHERE provider_id = ?1 AND account_id = ?2
               ORDER BY collected_at DESC, rowid DESC
               LIMIT -1 OFFSET ?4
             )
           )";
    conn.execute(
        &format!("DELETE FROM raw_payloads WHERE snapshot_id IN ({old_snapshot_ids})"),
        params![
            provider_id.as_str(),
            account_id.as_str(),
            retention_cutoff.to_rfc3339(),
            max_snapshots,
        ],
    )?;
    conn.execute(
        &format!("DELETE FROM usage_snapshots WHERE id IN ({old_snapshot_ids})"),
        params![
            provider_id.as_str(),
            account_id.as_str(),
            retention_cutoff.to_rfc3339(),
            max_snapshots,
        ],
    )?;
    Ok(())
}

fn usage_snapshot_from_row(row: &Row<'_>) -> rusqlite::Result<UsageSnapshot> {
    let json: String = row.get(0)?;
    serde_json::from_str(&json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn account_from_row(row: &Row<'_>) -> rusqlite::Result<Account> {
    let profile_id: String = row.get(3)?;
    let display_name_source: String = row.get(5)?;
    let created_at: String = row.get(9)?;
    let updated_at: String = row.get(10)?;
    Ok(Account {
        id: AccountId::new(row.get::<_, String>(0)?),
        provider_id: ProviderId::new(row.get::<_, String>(1)?),
        external_account_id: row.get(2)?,
        profile_id: (!profile_id.is_empty()).then_some(profile_id),
        display_name: row.get(4)?,
        display_name_source: display_name_source_from_sql(&display_name_source),
        email: row.get(6)?,
        hidden: row.get::<_, i64>(7)? != 0,
        collection_enabled: row.get::<_, i64>(8)? != 0,
        created_at: parse_time_sql(&created_at)?,
        updated_at: parse_time_sql(&updated_at)?,
    })
}

fn account_select_sql(suffix: &str) -> String {
    format!(
        "SELECT id, provider_id, external_account_id, profile_id, display_name,
                display_name_source, email, hidden, collection_enabled, created_at, updated_at
         FROM accounts
         {suffix}"
    )
}

fn normalized_profile_id(profile_id: Option<&str>, external_account_id: &str) -> String {
    profile_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(external_account_id)
        .to_string()
}

fn provider_requires_unique_external_account(provider_id: &ProviderId) -> bool {
    matches!(provider_id.as_str(), "codex" | "claude")
}

fn can_adopt_legacy_external_identity(
    provider_id: &ProviderId,
    stored_external_account_id: &str,
    discovered_external_account_id: &str,
) -> bool {
    provider_id.as_str() == "claude"
        && !is_canonical_uuid(stored_external_account_id)
        && is_canonical_uuid(discovered_external_account_id)
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .is_ok_and(|uuid| uuid.hyphenated().to_string().eq_ignore_ascii_case(value))
}

fn normalized_identity_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalized_email(value: Option<&str>) -> Option<String> {
    normalized_identity_value(value).filter(|value| looks_like_email(value))
}

fn looks_like_email(value: &str) -> bool {
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.chars().any(char::is_whitespace)
}

fn is_legacy_provider_display_name(
    provider_id: &str,
    external_account_id: &str,
    display_name: &str,
) -> bool {
    let display_name = display_name.trim();
    if display_name.eq_ignore_ascii_case(external_account_id.trim()) {
        return true;
    }
    match provider_id {
        "codex" => {
            display_name.eq_ignore_ascii_case("Codex")
                || display_name
                    .strip_prefix("Codex Account ")
                    .is_some_and(|suffix| suffix.parse::<u64>().is_ok())
        }
        "claude" => {
            display_name.eq_ignore_ascii_case("Claude")
                || display_name
                    .to_ascii_lowercase()
                    .strip_prefix("claude ")
                    .is_some_and(|suffix| {
                        matches!(suffix, "free" | "pro" | "max" | "team" | "enterprise")
                    })
        }
        "opencode_go" => display_name.eq_ignore_ascii_case("OpenCode Go local"),
        _ => false,
    }
}

fn display_name_source_sql(source: AccountDisplayNameSource) -> &'static str {
    match source {
        AccountDisplayNameSource::Provider => "provider",
        AccountDisplayNameSource::Generated => "generated",
        AccountDisplayNameSource::User => "user",
    }
}

fn display_name_source_from_sql(value: &str) -> AccountDisplayNameSource {
    match value {
        "user" => AccountDisplayNameSource::User,
        "provider" => AccountDisplayNameSource::Provider,
        _ => AccountDisplayNameSource::Generated,
    }
}

fn generated_account_display_name(conn: &Connection, provider_id: &str) -> anyhow::Result<String> {
    let mut stmt = conn.prepare("SELECT display_name FROM accounts WHERE provider_id = ?1")?;
    let existing = stmt
        .query_map(params![provider_id], |row| row.get::<_, Option<String>>(0))?
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;

    let (base, always_numbered) = match provider_id {
        "codex" => ("Codex".to_string(), true),
        "claude" => ("Claude".to_string(), true),
        "opencode_go" => ("OpenCode Go".to_string(), false),
        value => (
            value
                .split(['_', '-'])
                .filter(|part| !part.is_empty())
                .map(|part| {
                    let mut chars = part.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" "),
            true,
        ),
    };

    if !always_numbered && !existing.iter().any(|name| name.eq_ignore_ascii_case(&base)) {
        return Ok(base);
    }
    for ordinal in 1_u64.. {
        let candidate = format!("{base} {ordinal}");
        if !existing
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&candidate))
        {
            return Ok(candidate);
        }
    }
    unreachable!("account label ordinal space is exhausted")
}

fn migrate_account_profile_identity(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    if columns.iter().any(|column| column == "profile_id") {
        return Ok(());
    }

    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         BEGIN;
         CREATE TABLE accounts_new (
           id TEXT PRIMARY KEY,
           provider_id TEXT NOT NULL,
           external_account_id TEXT NOT NULL,
           profile_id TEXT NOT NULL DEFAULT '',
           display_name TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           UNIQUE(provider_id, profile_id)
         );
         INSERT INTO accounts_new
           (id, provider_id, external_account_id, profile_id, display_name, created_at, updated_at)
         SELECT
           id, provider_id, external_account_id, external_account_id, display_name, created_at, updated_at
         FROM accounts;
         DROP TABLE accounts;
         ALTER TABLE accounts_new RENAME TO accounts;
         CREATE INDEX IF NOT EXISTS accounts_provider_external_account
         ON accounts(provider_id, external_account_id);
         COMMIT;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}

fn migrate_account_lifecycle_state(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    if !columns.iter().any(|column| column == "hidden") {
        conn.execute(
            "ALTER TABLE accounts ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !columns.iter().any(|column| column == "collection_enabled") {
        conn.execute(
            "ALTER TABLE accounts ADD COLUMN collection_enabled INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    Ok(())
}

fn migrate_account_identity(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let had_source = columns.iter().any(|column| column == "display_name_source");
    let had_email = columns.iter().any(|column| column == "email");
    if !had_source && !had_email {
        conn.execute_batch(ACCOUNT_IDENTITY_MIGRATION)?;
    } else {
        if !had_source {
            conn.execute(
                "ALTER TABLE accounts ADD COLUMN display_name_source TEXT NOT NULL DEFAULT 'provider'",
                [],
            )?;
        }
        if !had_email {
            conn.execute("ALTER TABLE accounts ADD COLUMN email TEXT", [])?;
        }
    }
    let mut stmt = conn.prepare(
        "SELECT id, provider_id, external_account_id, display_name, email
         FROM accounts
         WHERE display_name_source = 'provider'
         ORDER BY provider_id, created_at, id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for (id, provider_id, external_account_id, display_name, email) in rows {
        let display_name = display_name.and_then(|value| normalized_identity_value(Some(&value)));
        let legacy_email = display_name
            .as_deref()
            .filter(|value| looks_like_email(value))
            .map(ToOwned::to_owned);
        let (display_name, source) = match display_name.filter(|value| {
            !looks_like_email(value)
                && !is_legacy_provider_display_name(&provider_id, &external_account_id, value)
        }) {
            Some(display_name) => (display_name, AccountDisplayNameSource::User),
            None => (
                generated_account_display_name(conn, &provider_id)?,
                AccountDisplayNameSource::Generated,
            ),
        };
        conn.execute(
            "UPDATE accounts
             SET display_name = ?1, display_name_source = ?2, email = ?3
             WHERE id = ?4",
            params![
                display_name,
                display_name_source_sql(source),
                email.or(legacy_email),
                id
            ],
        )?;
    }
    Ok(())
}

fn provider_health_from_row(row: &Row<'_>) -> rusqlite::Result<ProviderHealth> {
    let account_id: String = row.get(1)?;
    let last_success_at: Option<String> = row.get(4)?;
    let last_failure_at: Option<String> = row.get(5)?;
    let updated_at: String = row.get(8)?;
    Ok(ProviderHealth {
        provider_id: ProviderId::new(row.get::<_, String>(0)?),
        account_id: (!account_id.is_empty()).then(|| AccountId::new(account_id)),
        status: health_status_from_sql(&row.get::<_, String>(2)?),
        collection_mode: row.get(3)?,
        last_success_at: parse_optional_time_sql(last_success_at)?,
        last_failure_at: parse_optional_time_sql(last_failure_at)?,
        last_error_code: row.get(6)?,
        last_error_message: row.get(7)?,
        updated_at: parse_time_sql(&updated_at)?,
    })
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

fn health_status_to_sql(status: &ProviderHealthStatus) -> &'static str {
    match status {
        ProviderHealthStatus::Ok => "ok",
        ProviderHealthStatus::CredentialsMissing => "credentials_missing",
        ProviderHealthStatus::AuthFailed => "auth_failed",
        ProviderHealthStatus::RateLimited => "rate_limited",
        ProviderHealthStatus::ProviderError => "provider_error",
        ProviderHealthStatus::ParseError => "parse_error",
        ProviderHealthStatus::BackingOff => "backing_off",
        ProviderHealthStatus::Disabled => "disabled",
    }
}

fn health_status_from_sql(value: &str) -> ProviderHealthStatus {
    match value {
        "ok" => ProviderHealthStatus::Ok,
        "credentials_missing" => ProviderHealthStatus::CredentialsMissing,
        "auth_failed" => ProviderHealthStatus::AuthFailed,
        "rate_limited" => ProviderHealthStatus::RateLimited,
        "parse_error" => ProviderHealthStatus::ParseError,
        "backing_off" => ProviderHealthStatus::BackingOff,
        "disabled" => ProviderHealthStatus::Disabled,
        _ => ProviderHealthStatus::ProviderError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use usage_core::{UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

    #[tokio::test]
    async fn stores_and_reads_accounts_snapshots_and_health() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, Some("Codex"), None)
            .await
            .unwrap();

        let snapshot = UsageSnapshot {
            provider_id: provider_id.clone(),
            account_id: account.id.clone(),
            collected_at: Utc::now(),
            windows: vec![UsageWindow {
                window_id: "codex_session".to_string(),
                label: "Codex session".to_string(),
                kind: UsageWindowKind::Session,
                used: Some(UsageAmount {
                    value: 25.0,
                    unit: UsageUnit::Percent,
                }),
                limit: Some(UsageAmount {
                    value: 100.0,
                    unit: UsageUnit::Percent,
                }),
                remaining: Some(UsageAmount {
                    value: 75.0,
                    unit: UsageUnit::Percent,
                }),
                percent_used: Some(25.0),
                percent_remaining: Some(75.0),
                reset_at: None,
            }],
            metadata: json!({"collection_mode": "test"}),
        };
        storage
            .insert_snapshot(&snapshot, Some(&json!({"raw": true})))
            .await
            .unwrap();

        storage
            .upsert_health(&ProviderHealth {
                provider_id: provider_id.clone(),
                account_id: Some(account.id.clone()),
                status: ProviderHealthStatus::Ok,
                collection_mode: Some("test".to_string()),
                last_success_at: Some(Utc::now()),
                last_failure_at: None,
                last_error_code: None,
                last_error_message: None,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].external_account_id, "external-account");
        assert_eq!(accounts[0].profile_id.as_deref(), Some("external-account"));

        let snapshots = storage.latest_usage().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].account_id, account.id);
        assert_eq!(snapshots[0].windows[0].window_id, "codex_session");

        let health = storage.provider_health().await.unwrap();
        assert_eq!(health.len(), 1);
        assert!(matches!(health[0].status, ProviderHealthStatus::Ok));
        assert_eq!(health[0].collection_mode.as_deref(), Some("test"));
    }

    #[tokio::test]
    async fn recent_usage_is_bounded_filtered_and_newest_first() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, None, None)
            .await
            .unwrap();
        let start = Utc.with_ymd_and_hms(2026, 7, 10, 10, 0, 0).unwrap();
        let mut snapshot = UsageSnapshot {
            provider_id: provider_id.clone(),
            account_id: account.id.clone(),
            collected_at: start,
            windows: Vec::new(),
            metadata: json!({}),
        };
        for offset in [0, 5, 10] {
            snapshot.collected_at = start + chrono::TimeDelta::minutes(offset);
            storage.insert_snapshot(&snapshot, None).await.unwrap();
        }

        let recent = storage
            .recent_usage(
                &provider_id,
                &account.id,
                start + chrono::TimeDelta::minutes(4),
                10,
            )
            .await
            .unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent[0].collected_at,
            start + chrono::TimeDelta::minutes(10)
        );
        assert_eq!(
            recent[1].collected_at,
            start + chrono::TimeDelta::minutes(5)
        );

        let limited = storage
            .recent_usage(&provider_id, &account.id, start, 1)
            .await
            .unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].collected_at, recent[0].collected_at);
    }

    #[test]
    fn creates_private_database_files() {
        let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));

        let storage = Storage::open(&path).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for sidecar in [
            path.with_extension("sqlite3-shm"),
            path.with_extension("sqlite3-wal"),
        ] {
            if sidecar.exists() {
                assert_eq!(
                    std::fs::metadata(sidecar).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        drop(storage);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    }

    #[tokio::test]
    async fn upserts_and_retains_daily_usage_by_account_and_date() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(
                &provider_id,
                "external-account",
                Some("personal"),
                None,
                None,
            )
            .await
            .unwrap();
        let first_date = chrono::NaiveDate::from_ymd_opt(2026, 7, 8).unwrap();
        let second_date = chrono::NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();

        storage
            .upsert_daily_usage(
                &provider_id,
                &account.id,
                &[
                    DailyUsageBucket {
                        date: first_date,
                        tokens: 10,
                        cost_usd: None,
                        source: "codex_account_usage".to_string(),
                    },
                    DailyUsageBucket {
                        date: second_date,
                        tokens: 20,
                        cost_usd: None,
                        source: "codex_account_usage".to_string(),
                    },
                ],
                Utc::now(),
            )
            .await
            .unwrap();
        storage
            .upsert_daily_usage(
                &provider_id,
                &account.id,
                &[DailyUsageBucket {
                    date: second_date,
                    tokens: 25,
                    cost_usd: None,
                    source: "codex_account_usage".to_string(),
                }],
                Utc::now(),
            )
            .await
            .unwrap();

        let rows = storage.daily_usage_history().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tokens, 10);
        assert_eq!(rows[1].tokens, 25);

        let dashboard = storage.daily_usage_dashboard(second_date).await.unwrap();
        assert_eq!(dashboard.len(), 1);
        assert_eq!(dashboard[0].bucket_count, 2);
        assert_eq!(dashboard[0].total_tokens, 35);
        assert_eq!(dashboard[0].recent.len(), 1);
        assert_eq!(dashboard[0].recent[0].tokens, 25);

        storage.delete_account(&account.id).await.unwrap();
        assert!(storage.daily_usage_history().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deletes_provider_level_health_without_touching_account_health() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        let account_id = AccountId::new("account-id");

        storage
            .upsert_health(&ProviderHealth {
                provider_id: provider_id.clone(),
                account_id: None,
                status: ProviderHealthStatus::CredentialsMissing,
                collection_mode: None,
                last_success_at: None,
                last_failure_at: Some(Utc::now()),
                last_error_code: Some("credentials_missing".to_string()),
                last_error_message: Some("missing".to_string()),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();
        storage
            .upsert_health(&ProviderHealth {
                provider_id: provider_id.clone(),
                account_id: Some(account_id.clone()),
                status: ProviderHealthStatus::Ok,
                collection_mode: Some("live".to_string()),
                last_success_at: Some(Utc::now()),
                last_failure_at: None,
                last_error_code: None,
                last_error_message: None,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        storage
            .delete_provider_level_health(&provider_id)
            .await
            .unwrap();

        let health = storage.provider_health().await.unwrap();
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].account_id.as_ref(), Some(&account_id));
        assert!(matches!(health[0].status, ProviderHealthStatus::Ok));
    }

    #[tokio::test]
    async fn permanently_deletes_account_and_related_data() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(
                &provider_id,
                "external-account",
                Some("work"),
                Some("Work"),
                None,
            )
            .await
            .unwrap();
        storage
            .insert_snapshot(
                &UsageSnapshot {
                    provider_id: provider_id.clone(),
                    account_id: account.id.clone(),
                    collected_at: Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({}),
                },
                Some(&json!({"raw": true})),
            )
            .await
            .unwrap();
        storage
            .upsert_health(&ProviderHealth {
                provider_id,
                account_id: Some(account.id.clone()),
                status: ProviderHealthStatus::Ok,
                collection_mode: Some("test".to_string()),
                last_success_at: Some(Utc::now()),
                last_failure_at: None,
                last_error_code: None,
                last_error_message: None,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        storage.delete_account(&account.id).await.unwrap();

        assert!(storage.accounts().await.unwrap().is_empty());
        assert!(storage.latest_usage().await.unwrap().is_empty());
        assert!(storage.provider_health().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn returns_provider_ids_with_account_or_snapshot_data() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, Some("Codex"), None)
            .await
            .unwrap();

        storage
            .upsert_health(&ProviderHealth {
                provider_id: ProviderId::new("claude"),
                account_id: None,
                status: ProviderHealthStatus::Disabled,
                collection_mode: None,
                last_success_at: None,
                last_failure_at: None,
                last_error_code: None,
                last_error_message: None,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        storage
            .insert_snapshot(
                &UsageSnapshot {
                    provider_id: provider_id.clone(),
                    account_id: account.id,
                    collected_at: Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({}),
                },
                None,
            )
            .await
            .unwrap();

        let providers = storage.provider_data_ids().await.unwrap();
        assert_eq!(providers, vec![provider_id]);
    }

    #[tokio::test]
    async fn rejects_same_codex_external_account_for_distinct_profiles() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");

        storage
            .upsert_account(
                &provider_id,
                "same-openai-account",
                Some("personal"),
                Some("Personal"),
                None,
            )
            .await
            .unwrap();
        let error = storage
            .upsert_account(
                &provider_id,
                "same-openai-account",
                Some("work"),
                Some("Work"),
                None,
            )
            .await
            .unwrap_err();

        let conflict = error.downcast_ref::<AccountIdentityConflict>().unwrap();
        assert_eq!(
            conflict,
            &AccountIdentityConflict::DuplicateExternalAccount {
                provider_id: "codex".to_string(),
                external_account_id: "same-openai-account".to_string(),
                existing_profile_id: "personal".to_string(),
                discovered_profile_id: "work".to_string(),
            }
        );
        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].profile_id.as_deref(), Some("personal"));
    }

    #[tokio::test]
    async fn rejects_changing_the_external_account_for_an_existing_profile() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        let first_account_id = "11111111-1111-4111-8111-111111111111";
        let second_account_id = "22222222-2222-4222-8222-222222222222";
        let original = storage
            .upsert_account(
                &provider_id,
                first_account_id,
                Some("personal"),
                Some("Personal"),
                None,
            )
            .await
            .unwrap();

        let error = storage
            .upsert_account(
                &provider_id,
                second_account_id,
                Some("personal"),
                Some("Renamed"),
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<AccountIdentityConflict>(),
            Some(AccountIdentityConflict::ProfileChanged {
                stored_external_account_id,
                discovered_external_account_id,
                ..
            }) if stored_external_account_id == first_account_id
                && discovered_external_account_id == second_account_id
        ));
        let stored = storage.account(&original.id).await.unwrap().unwrap();
        assert_eq!(stored.external_account_id, first_account_id);
        assert_eq!(stored.display_name.as_deref(), Some("Personal"));
    }

    #[tokio::test]
    async fn upgrades_a_legacy_claude_identity_to_an_account_uuid_once() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        let account_uuid = "11111111-1111-4111-8111-111111111111";
        let legacy = storage
            .upsert_account(&provider_id, "macos-user", Some("default"), None, None)
            .await
            .unwrap();

        let upgraded = storage
            .upsert_account(&provider_id, account_uuid, Some("default"), None, None)
            .await
            .unwrap();

        assert_eq!(upgraded.id, legacy.id);
        assert_eq!(upgraded.external_account_id, account_uuid);
        assert_eq!(storage.accounts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_a_legacy_claude_upgrade_when_uuid_is_connected_elsewhere() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        let account_uuid = "11111111-1111-4111-8111-111111111111";
        storage
            .upsert_account(&provider_id, "macos-user", Some("default"), None, None)
            .await
            .unwrap();
        storage
            .upsert_account(&provider_id, account_uuid, Some("work"), None, None)
            .await
            .unwrap();

        let error = storage
            .upsert_account(&provider_id, account_uuid, Some("default"), None, None)
            .await
            .unwrap_err();

        assert_eq!(
            error.downcast_ref::<AccountIdentityConflict>(),
            Some(&AccountIdentityConflict::DuplicateExternalAccount {
                provider_id: "claude".to_string(),
                external_account_id: account_uuid.to_string(),
                existing_profile_id: "work".to_string(),
                discovered_profile_id: "default".to_string(),
            })
        );
        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().any(|account| {
            account.profile_id.as_deref() == Some("default")
                && account.external_account_id == "macos-user"
        }));
    }

    #[tokio::test]
    async fn legacy_duplicate_accounts_can_still_be_rediscovered() {
        let storage = test_storage();
        storage
            .with_connection(|conn| {
                let now = Utc::now().to_rfc3339();
                for profile_id in ["personal", "work"] {
                    conn.execute(
                        "INSERT INTO accounts
                         (id, provider_id, external_account_id, profile_id, display_name,
                          display_name_source, email, hidden, collection_enabled, created_at,
                          updated_at)
                         VALUES (?1, 'codex', 'duplicate', ?2, NULL, 'generated', NULL, 0, 1,
                                 ?3, ?3)",
                        params![Uuid::new_v4().to_string(), profile_id, now],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();

        let rediscovered = storage
            .upsert_account(
                &ProviderId::new("codex"),
                "duplicate",
                Some("work"),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(rediscovered.profile_id.as_deref(), Some("work"));
        assert_eq!(storage.accounts().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn account_lifecycle_state_survives_discovery_upsert() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(
                &provider_id,
                "external-account",
                Some("work"),
                Some("Work"),
                None,
            )
            .await
            .unwrap();

        let updated = storage
            .update_account(&account.id, Some("Renamed"), Some(true), Some(false))
            .await
            .unwrap();
        assert!(updated.hidden);
        assert!(!updated.collection_enabled);

        let rediscovered = storage
            .upsert_account(
                &provider_id,
                "external-account",
                Some("work"),
                Some("Work"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(rediscovered.id, account.id);
        assert!(rediscovered.hidden);
        assert!(!rediscovered.collection_enabled);
        assert_eq!(rediscovered.display_name.as_deref(), Some("Renamed"));
    }

    #[tokio::test]
    async fn latest_usage_breaks_timestamp_ties_deterministically() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, None, None)
            .await
            .unwrap();
        let collected_at = Utc::now();
        for version in [1, 2] {
            storage
                .insert_snapshot(
                    &UsageSnapshot {
                        provider_id: provider_id.clone(),
                        account_id: account.id.clone(),
                        collected_at,
                        windows: Vec::new(),
                        metadata: json!({"version": version}),
                    },
                    None,
                )
                .await
                .unwrap();
        }

        let snapshots = storage.latest_usage().await.unwrap();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].metadata["version"], 2);
    }

    #[tokio::test]
    async fn prunes_bounded_snapshot_and_raw_payload_history() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, None, None)
            .await
            .unwrap();
        for version in 1..=3 {
            storage
                .insert_snapshot(
                    &UsageSnapshot {
                        provider_id: provider_id.clone(),
                        account_id: account.id.clone(),
                        collected_at: Utc::now(),
                        windows: Vec::new(),
                        metadata: json!({"version": version}),
                    },
                    Some(&json!({"version": version})),
                )
                .await
                .unwrap();
        }

        let prune_provider_id = provider_id.clone();
        let prune_account_id = account.id.clone();
        storage
            .with_connection(move |conn| {
                prune_account_history(
                    conn,
                    &prune_provider_id,
                    &prune_account_id,
                    Utc::now() - chrono::TimeDelta::days(90),
                    2,
                    1,
                )
            })
            .await
            .unwrap();

        let account_id = account.id.clone();
        let (snapshots, raw_payloads) = storage
            .with_connection(move |conn| {
                let snapshots: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM usage_snapshots WHERE account_id = ?1",
                    params![account_id.as_str()],
                    |row| row.get(0),
                )?;
                let raw_payloads: i64 =
                    conn.query_row("SELECT COUNT(*) FROM raw_payloads", [], |row| row.get(0))?;
                Ok((snapshots, raw_payloads))
            })
            .await
            .unwrap();
        assert_eq!(snapshots, 2);
        assert_eq!(raw_payloads, 1);
        assert_eq!(
            storage.latest_usage().await.unwrap()[0].metadata["version"],
            3
        );
    }

    #[tokio::test]
    async fn generated_names_are_short_and_user_names_survive_provider_updates() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let personal = storage
            .upsert_account(
                &provider_id,
                "personal-id",
                Some("personal"),
                None,
                Some("personal@example.com"),
            )
            .await
            .unwrap();
        let work = storage
            .upsert_account(
                &provider_id,
                "work-id",
                Some("work"),
                None,
                Some("work@example.com"),
            )
            .await
            .unwrap();

        assert_eq!(personal.display_name.as_deref(), Some("Codex 1"));
        assert_eq!(work.display_name.as_deref(), Some("Codex 2"));
        assert_eq!(personal.email.as_deref(), Some("personal@example.com"));
        assert_eq!(
            personal.display_name_source,
            AccountDisplayNameSource::Generated
        );

        storage
            .update_account(&personal.id, Some("Personal"), None, None)
            .await
            .unwrap();
        let rediscovered = storage
            .upsert_account(
                &provider_id,
                "personal-id",
                Some("personal"),
                Some("provider replacement"),
                Some("new-personal@example.com"),
            )
            .await
            .unwrap();

        assert_eq!(rediscovered.display_name.as_deref(), Some("Personal"));
        assert_eq!(
            rediscovered.email.as_deref(),
            Some("new-personal@example.com")
        );
        assert_eq!(
            rediscovered.display_name_source,
            AccountDisplayNameSource::User
        );
    }

    #[tokio::test]
    async fn user_name_survives_database_reopen_and_rediscovery() {
        let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));
        let provider_id = ProviderId::new("codex");
        let storage = Storage::open(&path).unwrap();
        let account = storage
            .upsert_account(
                &provider_id,
                "external",
                Some("default"),
                None,
                Some("first@example.com"),
            )
            .await
            .unwrap();
        storage
            .update_account(&account.id, Some("Personal"), None, None)
            .await
            .unwrap();
        drop(storage);

        let storage = Storage::open(&path).unwrap();
        let rediscovered = storage
            .upsert_account(
                &provider_id,
                "external",
                Some("default"),
                None,
                Some("second@example.com"),
            )
            .await
            .unwrap();
        assert_eq!(rediscovered.display_name.as_deref(), Some("Personal"));
        assert_eq!(rediscovered.email.as_deref(), Some("second@example.com"));
        assert_eq!(
            rediscovered.display_name_source,
            AccountDisplayNameSource::User
        );
        drop(storage);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn migrates_legacy_email_names_into_separate_identity_fields() {
        let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
               id TEXT PRIMARY KEY,
               provider_id TEXT NOT NULL,
               external_account_id TEXT NOT NULL,
               profile_id TEXT NOT NULL DEFAULT '',
               display_name TEXT,
               hidden INTEGER NOT NULL DEFAULT 0,
               collection_enabled INTEGER NOT NULL DEFAULT 1,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               UNIQUE(provider_id, profile_id)
             );",
        )
        .unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO accounts
             (id, provider_id, external_account_id, profile_id, display_name, created_at, updated_at)
             VALUES ('account', 'codex', 'external', 'default', 'legacy@example.com', ?1, ?1)",
            params![now],
        )
        .unwrap();
        drop(conn);

        let storage = Storage::open(&path).unwrap();
        let account = storage
            .account(&AccountId::new("account"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.display_name.as_deref(), Some("Codex 1"));
        assert_eq!(account.email.as_deref(), Some("legacy@example.com"));
        assert_eq!(
            account.display_name_source,
            AccountDisplayNameSource::Generated
        );
        drop(storage);
        let _ = std::fs::remove_file(path);
    }

    fn test_storage() -> Storage {
        let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
