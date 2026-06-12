use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use usage_core::{
    Account, AccountId, ProviderHealth, ProviderHealthStatus, ProviderId, UsageSnapshot,
};
use uuid::Uuid;

const INITIAL_MIGRATION: &str = include_str!("../migrations/0001_initial.sql");

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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn upsert_account(
        &self,
        provider_id: &ProviderId,
        external_account_id: &str,
        display_name: Option<&str>,
    ) -> anyhow::Result<Account> {
        let now = Utc::now();
        let conn = self.lock()?;
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, created_at FROM accounts WHERE provider_id = ?1 AND external_account_id = ?2",
                params![provider_id.as_str(), external_account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let (id, created_at) =
            existing.unwrap_or_else(|| (Uuid::new_v4().to_string(), now.to_rfc3339()));
        conn.execute(
            "INSERT INTO accounts (id, provider_id, external_account_id, display_name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(provider_id, external_account_id) DO UPDATE SET
               display_name = excluded.display_name,
               updated_at = excluded.updated_at",
            params![
                id,
                provider_id.as_str(),
                external_account_id,
                display_name,
                created_at,
                now.to_rfc3339(),
            ],
        )?;

        Ok(Account {
            id: AccountId::new(id),
            provider_id: provider_id.clone(),
            external_account_id: external_account_id.to_string(),
            display_name: display_name.map(ToOwned::to_owned),
            created_at: parse_time(&created_at)?,
            updated_at: now,
        })
    }

    pub fn insert_snapshot(
        &self,
        snapshot: &UsageSnapshot,
        raw_payload: Option<&serde_json::Value>,
    ) -> anyhow::Result<()> {
        let snapshot_id = Uuid::new_v4().to_string();
        let normalized_json = serde_json::to_string(snapshot)?;
        let metadata_json = serde_json::to_string(&snapshot.metadata)?;
        let conn = self.lock()?;
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
                    serde_json::to_string(raw_payload)?,
                ],
            )?;
        }

        Ok(())
    }

    pub fn upsert_health(&self, health: &ProviderHealth) -> anyhow::Result<()> {
        let account_id = health
            .account_id
            .as_ref()
            .map(AccountId::as_str)
            .unwrap_or("");
        let conn = self.lock()?;
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
                status_to_str(&health.status),
                health.collection_mode.as_deref(),
                health.last_success_at.map(|time| time.to_rfc3339()),
                health.last_failure_at.map(|time| time.to_rfc3339()),
                health.last_error_code.as_deref(),
                health.last_error_message.as_deref(),
                health.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn latest_usage(&self) -> anyhow::Result<Vec<UsageSnapshot>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT normalized_json FROM usage_snapshots s
             WHERE collected_at = (
               SELECT MAX(collected_at) FROM usage_snapshots
               WHERE provider_id = s.provider_id AND account_id = s.account_id
             )
             ORDER BY provider_id, account_id",
        )?;
        let snapshots = stmt
            .query_map([], |row| {
                let json: String = row.get(0)?;
                let snapshot = serde_json::from_str(&json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?;
                Ok(snapshot)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(snapshots)
    }

    pub fn accounts(&self) -> anyhow::Result<Vec<Account>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, provider_id, external_account_id, display_name, created_at, updated_at
             FROM accounts
             ORDER BY provider_id, external_account_id",
        )?;
        let accounts = stmt
            .query_map([], |row| {
                let created_at: String = row.get(4)?;
                let updated_at: String = row.get(5)?;
                Ok(Account {
                    id: AccountId::new(row.get::<_, String>(0)?),
                    provider_id: ProviderId::new(row.get::<_, String>(1)?),
                    external_account_id: row.get(2)?,
                    display_name: row.get(3)?,
                    created_at: parse_time_sql(&created_at)?,
                    updated_at: parse_time_sql(&updated_at)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(accounts)
    }

    pub fn provider_health(&self) -> anyhow::Result<Vec<ProviderHealth>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT provider_id, account_id, status, collection_mode, last_success_at,
                    last_failure_at, last_error_code, last_error_message, updated_at
             FROM provider_health
             ORDER BY provider_id, account_id",
        )?;
        let health = stmt
            .query_map([], |row| {
                let account_id: String = row.get(1)?;
                let last_success_at: Option<String> = row.get(4)?;
                let last_failure_at: Option<String> = row.get(5)?;
                let updated_at: String = row.get(8)?;
                Ok(ProviderHealth {
                    provider_id: ProviderId::new(row.get::<_, String>(0)?),
                    account_id: (!account_id.is_empty()).then(|| AccountId::new(account_id)),
                    status: str_to_status(&row.get::<_, String>(2)?),
                    collection_mode: row.get(3)?,
                    last_success_at: parse_optional_time_sql(last_success_at)?,
                    last_failure_at: parse_optional_time_sql(last_failure_at)?,
                    last_error_code: row.get(6)?,
                    last_error_message: row.get(7)?,
                    updated_at: parse_time_sql(&updated_at)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(health)
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection mutex poisoned"))
    }
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

fn status_to_str(status: &ProviderHealthStatus) -> &'static str {
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

fn str_to_status(value: &str) -> ProviderHealthStatus {
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
