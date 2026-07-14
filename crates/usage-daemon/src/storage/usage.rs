use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use usage_core::{AccountId, ProviderHealth, ProviderId, UsageSnapshot, UsageWindowKind};
use uuid::Uuid;

use crate::providers::DailyUsageBucket;

use super::{
    accounts::{accounts_from_conn, looks_like_email},
    backoff::{delete_provider_backoff_conn, upsert_provider_backoff_conn},
    health::{health_status_to_sql, provider_health_from_conn},
    parse_time_sql, Storage, StoredDailyUsage, StoredDailyUsageHistory, StoredForecastHistory,
    StoredProviderBackoff, StoredUsageDashboard, StoredWindowObservation,
    FORECAST_OBSERVATIONS_QUERY, MAX_SNAPSHOTS_PER_ACCOUNT, SNAPSHOT_RETENTION_DAYS,
    UPSERT_DAILY_USAGE_QUERY,
};

impl Storage {
    #[cfg(test)]
    pub async fn insert_snapshot(&self, snapshot: &UsageSnapshot) -> anyhow::Result<()> {
        let snapshot = snapshot.clone();
        self.with_connection(move |conn| {
            let snapshot_id = Uuid::new_v4().to_string();
            let normalized_json = serde_json::to_string(&snapshot)?;
            conn.execute(
                "INSERT INTO usage_snapshots
             (id, provider_id, account_id, collected_at, normalized_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    snapshot_id,
                    snapshot.provider_id.as_str(),
                    snapshot.account_id.as_str(),
                    snapshot.collected_at.to_rfc3339(),
                    normalized_json,
                ],
            )?;
            let snapshot_sequence = conn.last_insert_rowid();
            insert_window_observations(conn, &snapshot_id, snapshot_sequence, &snapshot)?;

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
            upsert_daily_usage_buckets(
                &transaction,
                &provider_id,
                &account_id,
                buckets,
                &collected_at.to_rfc3339(),
            )?;
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

    #[cfg(test)]
    pub async fn daily_usage_dashboard(
        &self,
        recent_since: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<StoredDailyUsageHistory>> {
        self.with_connection(move |conn| daily_usage_dashboard_from_conn(conn, recent_since))
            .await
    }

    pub async fn usage_dashboard(
        &self,
        recent_since: chrono::NaiveDate,
        forecast_since: DateTime<Utc>,
        forecast_limit: usize,
    ) -> anyhow::Result<StoredUsageDashboard> {
        let forecast_limit = forecast_limit.min(MAX_SNAPSHOTS_PER_ACCOUNT);
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let snapshots = latest_usage_from_conn(&transaction)?;
            let accounts = accounts_from_conn(&transaction)?;
            let health = provider_health_from_conn(&transaction)?;
            let daily_usage = daily_usage_dashboard_from_conn(&transaction, recent_since)?;
            let forecast_histories = forecast_histories_from_conn(
                &transaction,
                &snapshots,
                forecast_since,
                forecast_limit,
            )?;
            transaction.commit()?;
            Ok(StoredUsageDashboard {
                snapshots,
                accounts,
                health,
                daily_usage,
                forecast_histories,
            })
        })
        .await
    }

    pub async fn forecast_history(
        &self,
        snapshot: &UsageSnapshot,
        since: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<StoredForecastHistory> {
        let snapshot = snapshot.clone();
        let limit = limit.min(MAX_SNAPSHOTS_PER_ACCOUNT);
        self.with_connection(move |conn| {
            let key = (snapshot.provider_id.clone(), snapshot.account_id.clone());
            Ok(
                forecast_histories_from_conn(conn, &[snapshot], since, limit)?
                    .remove(&key)
                    .unwrap_or_default(),
            )
        })
        .await
    }

    pub async fn record_collection(
        &self,
        snapshot: &UsageSnapshot,
        daily_usage: &[DailyUsageBucket],
        health: &ProviderHealth,
        email: Option<&str>,
        backoff: Option<&StoredProviderBackoff>,
        clear_backoff: bool,
    ) -> anyhow::Result<()> {
        let snapshot = snapshot.clone();
        let daily_usage = daily_usage.to_vec();
        let health = health.clone();
        let backoff = backoff.cloned();
        let email = email
            .map(str::trim)
            .filter(|value| looks_like_email(value))
            .map(ToOwned::to_owned);
        self.with_connection(move |conn| {
            let snapshot_id = Uuid::new_v4().to_string();
            let normalized_json = serde_json::to_string(&snapshot)?;
            let collected_at = snapshot.collected_at.to_rfc3339();
            let transaction = conn.unchecked_transaction()?;

            upsert_daily_usage_buckets(
                &transaction,
                &snapshot.provider_id,
                &snapshot.account_id,
                daily_usage,
                &collected_at,
            )?;

            transaction.execute(
                "INSERT INTO usage_snapshots
                 (id, provider_id, account_id, collected_at, normalized_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    snapshot_id,
                    snapshot.provider_id.as_str(),
                    snapshot.account_id.as_str(),
                    collected_at,
                    normalized_json,
                ],
            )?;
            let snapshot_sequence = transaction.last_insert_rowid();
            insert_window_observations(
                &transaction,
                &snapshot_id,
                snapshot_sequence,
                &snapshot,
            )?;
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
            if let Some(backoff) = backoff.as_ref() {
                upsert_provider_backoff_conn(&transaction, backoff)?;
            } else if clear_backoff {
                delete_provider_backoff_conn(
                    &transaction,
                    &snapshot.provider_id,
                    &snapshot.account_id,
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
            )?;

            transaction.commit()?;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub async fn latest_usage(&self) -> anyhow::Result<Vec<UsageSnapshot>> {
        self.with_connection(latest_usage_from_conn).await
    }

    #[cfg(test)]
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
}

fn upsert_daily_usage_buckets(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: &AccountId,
    buckets: impl IntoIterator<Item = DailyUsageBucket>,
    collected_at: &str,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare_cached(UPSERT_DAILY_USAGE_QUERY)?;
    for bucket in buckets {
        let tokens = i64::try_from(bucket.tokens).map_err(|_| {
            anyhow::anyhow!(
                "daily usage tokens exceed SQLite integer range for {}",
                bucket.date
            )
        })?;
        stmt.execute(params![
            provider_id.as_str(),
            account_id.as_str(),
            bucket.date.to_string(),
            tokens,
            bucket.cost_usd,
            bucket.source,
            collected_at,
        ])?;
    }
    Ok(())
}

fn latest_usage_from_conn(conn: &Connection) -> anyhow::Result<Vec<UsageSnapshot>> {
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
}

fn daily_usage_dashboard_from_conn(
    conn: &Connection,
    recent_since: chrono::NaiveDate,
) -> anyhow::Result<Vec<StoredDailyUsageHistory>> {
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
        let (provider_id, account_id, bucket_count, total_tokens, date, tokens, cost_usd, source) =
            row?;
        let is_new_history = histories.last().is_none_or(|history| {
            history.provider_id.as_str() != provider_id || history.account_id.as_str() != account_id
        });
        if is_new_history {
            histories.push(StoredDailyUsageHistory {
                provider_id: ProviderId::new(provider_id.clone()),
                account_id: AccountId::new(account_id.clone()),
                bucket_count: usize::try_from(bucket_count).map_err(|err| {
                    anyhow::anyhow!("daily usage bucket count was invalid: {err}")
                })?,
                total_tokens: u64::try_from(total_tokens)
                    .map_err(|err| anyhow::anyhow!("daily usage total was invalid: {err}"))?,
                recent: Vec::new(),
            });
        }

        if let (Some(date), Some(tokens), Some(source)) = (date, tokens, source) {
            let history = histories
                .last_mut()
                .ok_or_else(|| anyhow::anyhow!("daily usage history was not initialized"))?;
            history.recent.push(StoredDailyUsage {
                provider_id: ProviderId::new(provider_id),
                account_id: AccountId::new(account_id),
                date: chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")?,
                tokens: u64::try_from(tokens)
                    .map_err(|err| anyhow::anyhow!("daily usage tokens were invalid: {err}"))?,
                cost_usd,
                source,
            });
        }
    }
    Ok(histories)
}

fn forecast_histories_from_conn(
    conn: &Connection,
    snapshots: &[UsageSnapshot],
    since: DateTime<Utc>,
    limit: usize,
) -> anyhow::Result<HashMap<(ProviderId, AccountId), StoredForecastHistory>> {
    let mut histories = HashMap::new();
    if limit == 0 {
        return Ok(histories);
    }
    let limit = i64::try_from(limit)?;
    let since = since.to_rfc3339();
    let mut stmt = conn.prepare(FORECAST_OBSERVATIONS_QUERY)?;

    for snapshot in snapshots {
        let key = (snapshot.provider_id.clone(), snapshot.account_id.clone());
        let history = histories
            .entry(key)
            .or_insert_with(StoredForecastHistory::default);
        for window in &snapshot.windows {
            if matches!(
                window.kind,
                UsageWindowKind::Credits | UsageWindowKind::Tokens
            ) || !window.percent_used.is_some_and(f64::is_finite)
            {
                continue;
            }
            let observations = stmt
                .query_map(
                    params![
                        snapshot.provider_id.as_str(),
                        snapshot.account_id.as_str(),
                        window.window_id.as_str(),
                        since.as_str(),
                        snapshot.collected_at.to_rfc3339(),
                        limit,
                    ],
                    |row| {
                        let collected_at: String = row.get(0)?;
                        let reset_at: Option<String> = row.get(2)?;
                        Ok(StoredWindowObservation {
                            collected_at: parse_time_sql(&collected_at)?,
                            percent_used: row.get(1)?,
                            reset_at: reset_at.as_deref().map(parse_time_sql).transpose()?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            history
                .by_window
                .insert(window.window_id.clone(), observations);
        }
    }
    Ok(histories)
}

fn insert_window_observations(
    conn: &Connection,
    snapshot_id: &str,
    snapshot_sequence: i64,
    snapshot: &UsageSnapshot,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO usage_window_observations
         (snapshot_id, snapshot_sequence, provider_id, account_id, window_id, collected_at,
          percent_used, reset_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let collected_at = snapshot.collected_at.to_rfc3339();
    for window in &snapshot.windows {
        let Some(percent_used) = window.percent_used.filter(|value| value.is_finite()) else {
            continue;
        };
        stmt.execute(params![
            snapshot_id,
            snapshot_sequence,
            snapshot.provider_id.as_str(),
            snapshot.account_id.as_str(),
            window.window_id.as_str(),
            collected_at.as_str(),
            percent_used,
            window.reset_at.map(|value| value.to_rfc3339()),
        ])?;
    }
    Ok(())
}

pub(super) fn prune_account_history(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: &AccountId,
    retention_cutoff: DateTime<Utc>,
    max_snapshots: usize,
) -> anyhow::Result<()> {
    let max_snapshots = i64::try_from(max_snapshots)?;
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
