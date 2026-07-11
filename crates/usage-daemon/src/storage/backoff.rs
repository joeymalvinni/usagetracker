use rusqlite::params;
use usage_core::{AccountId, ProviderId};

use super::{parse_time_sql, Storage, StoredProviderBackoff};

impl Storage {
    pub async fn provider_backoff(
        &self,
        provider_id: &ProviderId,
        account_id: &AccountId,
    ) -> anyhow::Result<Option<StoredProviderBackoff>> {
        let provider_id = provider_id.clone();
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT consecutive_failures, retry_at, last_failure_at, error_message
                 FROM provider_backoff WHERE provider_id = ?1 AND account_id = ?2",
            )?;
            let mut rows = stmt.query(params![provider_id.as_str(), account_id.as_str()])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let failures: i64 = row.get(0)?;
            let retry_at: String = row.get(1)?;
            let last_failure_at: String = row.get(2)?;
            Ok(Some(StoredProviderBackoff {
                provider_id,
                account_id,
                consecutive_failures: usize::try_from(failures)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, failures))?,
                retry_at: parse_time_sql(&retry_at)?,
                last_failure_at: parse_time_sql(&last_failure_at)?,
                error_message: row.get(3)?,
            }))
        })
        .await
    }

    pub async fn upsert_provider_backoff(
        &self,
        backoff: &StoredProviderBackoff,
    ) -> anyhow::Result<()> {
        let backoff = backoff.clone();
        self.with_connection(move |conn| {
            conn.execute(
                "INSERT INTO provider_backoff
                 (provider_id, account_id, consecutive_failures, retry_at,
                  last_failure_at, error_message)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(provider_id, account_id) DO UPDATE SET
                   consecutive_failures = excluded.consecutive_failures,
                   retry_at = excluded.retry_at,
                   last_failure_at = excluded.last_failure_at,
                   error_message = excluded.error_message",
                params![
                    backoff.provider_id.as_str(),
                    backoff.account_id.as_str(),
                    i64::try_from(backoff.consecutive_failures)?,
                    backoff.retry_at.to_rfc3339(),
                    backoff.last_failure_at.to_rfc3339(),
                    backoff.error_message,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn delete_provider_backoff(
        &self,
        provider_id: &ProviderId,
        account_id: &AccountId,
    ) -> anyhow::Result<()> {
        let provider_id = provider_id.clone();
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            conn.execute(
                "DELETE FROM provider_backoff WHERE provider_id = ?1 AND account_id = ?2",
                params![provider_id.as_str(), account_id.as_str()],
            )?;
            Ok(())
        })
        .await
    }
}
