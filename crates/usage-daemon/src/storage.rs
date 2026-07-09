use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use usage_core::{
    Account, AccountId, ProviderHealth, ProviderHealthStatus, ProviderId, UsageSnapshot,
};
use uuid::Uuid;

use crate::providers::DailyUsageBucket;

const INITIAL_MIGRATION: &str = include_str!("../migrations/0001_initial.sql");
const DAILY_USAGE_MIGRATION: &str = include_str!("../migrations/0002_provider_daily_usage.sql");

#[derive(Clone, Debug, PartialEq)]
pub struct StoredDailyUsage {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub date: chrono::NaiveDate,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    pub source: String,
}

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
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
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(INITIAL_MIGRATION)?;
        migrate_account_profile_identity(&conn)?;
        migrate_account_lifecycle_state(&conn)?;
        conn.execute_batch(DAILY_USAGE_MIGRATION)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn upsert_account(
        &self,
        provider_id: &ProviderId,
        external_account_id: &str,
        profile_id: Option<&str>,
        display_name: Option<&str>,
    ) -> anyhow::Result<Account> {
        let provider_id = provider_id.clone();
        let external_account_id = external_account_id.to_string();
        let profile_id = normalized_profile_id(profile_id, &external_account_id);
        let display_name = display_name.map(ToOwned::to_owned);
        self.with_connection(move |conn| {
        let now = Utc::now();
        let existing: Option<(String, String, bool, bool)> = conn
            .query_row(
                "SELECT id, created_at, hidden, collection_enabled
                 FROM accounts
                 WHERE provider_id = ?1 AND profile_id = ?2",
                params![provider_id.as_str(), profile_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()?;

        let (id, created_at, hidden, collection_enabled) = existing
            .unwrap_or_else(|| (Uuid::new_v4().to_string(), now.to_rfc3339(), false, true));
        conn.execute(
            "INSERT INTO accounts
             (id, provider_id, external_account_id, profile_id, display_name, hidden, collection_enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(provider_id, profile_id) DO UPDATE SET
               external_account_id = excluded.external_account_id,
               display_name = excluded.display_name,
               updated_at = excluded.updated_at",
            params![
                id,
                provider_id.as_str(),
                external_account_id.as_str(),
                profile_id.as_str(),
                display_name.as_deref(),
                i64::from(hidden),
                i64::from(collection_enabled),
                created_at,
                now.to_rfc3339(),
            ],
        )?;

        Ok(Account {
            id: AccountId::new(id),
            provider_id,
            external_account_id,
            profile_id: (!profile_id.is_empty()).then_some(profile_id),
            display_name,
            hidden,
            collection_enabled,
            created_at: parse_time(&created_at)?,
            updated_at: now,
        })
        })
        .await
    }

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

    pub async fn update_account_display_name(
        &self,
        account_id: &AccountId,
        display_name: &str,
    ) -> anyhow::Result<()> {
        let account_id = account_id.clone();
        let display_name = display_name.trim().to_string();
        self.with_connection(move |conn| {
            if display_name.is_empty() {
                return Ok(());
            }
            conn.execute(
                "UPDATE accounts SET display_name = ?1, updated_at = ?2 WHERE id = ?3",
                params![display_name, Utc::now().to_rfc3339(), account_id.as_str()],
            )?;
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
            let next_hidden = hidden.unwrap_or(existing.hidden);
            let next_collection_enabled = collection_enabled.unwrap_or(existing.collection_enabled);
            let updated_at = Utc::now();
            conn.execute(
                "UPDATE accounts
                 SET display_name = ?1,
                     hidden = ?2,
                     collection_enabled = ?3,
                     updated_at = ?4
                 WHERE id = ?5",
                params![
                    next_display_name,
                    i64::from(next_hidden),
                    i64::from(next_collection_enabled),
                    updated_at.to_rfc3339(),
                    account_id.as_str(),
                ],
            )?;
            Ok(Account {
                display_name: next_display_name.map(ToOwned::to_owned),
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
                "SELECT normalized_json FROM usage_snapshots s
             WHERE collected_at = (
               SELECT MAX(collected_at) FROM usage_snapshots
               WHERE provider_id = s.provider_id AND account_id = s.account_id
             )
             ORDER BY provider_id, account_id",
            )?;
            let snapshots = stmt
                .query_map([], usage_snapshot_from_row)?
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
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("sqlite connection mutex poisoned"))?;
            operation(&conn)
        })
        .await?
    }
}

fn usage_snapshot_from_row(row: &Row<'_>) -> rusqlite::Result<UsageSnapshot> {
    let json: String = row.get(0)?;
    serde_json::from_str(&json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn account_from_row(row: &Row<'_>) -> rusqlite::Result<Account> {
    let profile_id: String = row.get(3)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;
    Ok(Account {
        id: AccountId::new(row.get::<_, String>(0)?),
        provider_id: ProviderId::new(row.get::<_, String>(1)?),
        external_account_id: row.get(2)?,
        profile_id: (!profile_id.is_empty()).then_some(profile_id),
        display_name: row.get(4)?,
        hidden: row.get::<_, i64>(5)? != 0,
        collection_enabled: row.get::<_, i64>(6)? != 0,
        created_at: parse_time_sql(&created_at)?,
        updated_at: parse_time_sql(&updated_at)?,
    })
}

fn account_select_sql(suffix: &str) -> String {
    format!(
        "SELECT id, provider_id, external_account_id, profile_id, display_name,
                hidden, collection_enabled, created_at, updated_at
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

fn parse_time(value: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
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
    use serde_json::json;
    use usage_core::{UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

    #[tokio::test]
    async fn stores_and_reads_accounts_snapshots_and_health() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, Some("Codex"))
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
    async fn upserts_and_retains_daily_usage_by_account_and_date() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", Some("personal"), None)
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
            .upsert_account(&provider_id, "external-account", Some("work"), Some("Work"))
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
            .upsert_account(&provider_id, "external-account", None, Some("Codex"))
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
    async fn stores_same_external_account_for_distinct_profiles() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");

        let personal = storage
            .upsert_account(
                &provider_id,
                "same-openai-account",
                Some("personal"),
                Some("Personal"),
            )
            .await
            .unwrap();
        let work = storage
            .upsert_account(
                &provider_id,
                "same-openai-account",
                Some("work"),
                Some("Work"),
            )
            .await
            .unwrap();

        assert_ne!(personal.id, work.id);
        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts
            .iter()
            .all(|account| account.external_account_id == "same-openai-account"));
        assert!(accounts
            .iter()
            .any(|account| account.profile_id.as_deref() == Some("personal")));
        assert!(accounts
            .iter()
            .any(|account| account.profile_id.as_deref() == Some("work")));
    }

    #[tokio::test]
    async fn account_lifecycle_state_survives_discovery_upsert() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let account = storage
            .upsert_account(&provider_id, "external-account", Some("work"), Some("Work"))
            .await
            .unwrap();

        let updated = storage
            .update_account(&account.id, None, Some(true), Some(false))
            .await
            .unwrap();
        assert!(updated.hidden);
        assert!(!updated.collection_enabled);

        let rediscovered = storage
            .upsert_account(&provider_id, "external-account", Some("work"), Some("Work"))
            .await
            .unwrap();
        assert_eq!(rediscovered.id, account.id);
        assert!(rediscovered.hidden);
        assert!(!rediscovered.collection_enabled);
    }

    fn test_storage() -> Storage {
        let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
