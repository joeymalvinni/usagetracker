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

    pub async fn upsert_account(
        &self,
        provider_id: &ProviderId,
        external_account_id: &str,
        display_name: Option<&str>,
    ) -> anyhow::Result<Account> {
        let provider_id = provider_id.clone();
        let external_account_id = external_account_id.to_string();
        let display_name = display_name.map(ToOwned::to_owned);
        self.with_connection(move |conn| {
        let now = Utc::now();
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, created_at FROM accounts WHERE provider_id = ?1 AND external_account_id = ?2",
                params![provider_id.as_str(), external_account_id.as_str()],
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
                external_account_id.as_str(),
                display_name.as_deref(),
                created_at,
                now.to_rfc3339(),
            ],
        )?;

        Ok(Account {
            id: AccountId::new(id),
            provider_id,
            external_account_id,
            display_name,
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
                "SELECT id, provider_id, external_account_id, display_name, created_at, updated_at
             FROM accounts
             ORDER BY provider_id, external_account_id",
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
            .upsert_account(&provider_id, "external-account", Some("Codex"))
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

        let snapshots = storage.latest_usage().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].account_id, account.id);
        assert_eq!(snapshots[0].windows[0].window_id, "codex_session");

        let health = storage.provider_health().await.unwrap();
        assert_eq!(health.len(), 1);
        assert!(matches!(health[0].status, ProviderHealthStatus::Ok));
        assert_eq!(health[0].collection_mode.as_deref(), Some("test"));
    }

    fn test_storage() -> Storage {
        let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
