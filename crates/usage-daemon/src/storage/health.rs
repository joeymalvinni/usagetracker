use rusqlite::{params, Row};
use usage_core::{AccountId, ProviderHealth, ProviderHealthStatus, ProviderId};

use super::{parse_optional_time_sql, parse_time_sql, Storage};

impl Storage {
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
    pub async fn provider_health(&self) -> anyhow::Result<Vec<ProviderHealth>> {
        self.with_connection(provider_health_from_conn).await
    }
}

pub(super) fn provider_health_from_conn(
    conn: &rusqlite::Connection,
) -> anyhow::Result<Vec<ProviderHealth>> {
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

pub(super) fn health_status_to_sql(status: &ProviderHealthStatus) -> &'static str {
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
