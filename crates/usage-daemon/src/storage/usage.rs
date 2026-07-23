use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Local, Utc};
use rusqlite::{params, Connection, Row};
use serde::Deserialize;
use usage_core::{
    AccountId, DatasetProvenance, ProviderId, UsageEvent, UsageEventPage, UsageSnapshot,
    UsageWindowKind,
};
use uuid::Uuid;

use crate::providers::{DailyUsageBucket, ProviderUsageEventBatch, UsageDataset};

use super::{
    accounts::{accounts_from_conn, looks_like_email},
    backoff::{delete_provider_backoff_conn, upsert_provider_backoff_conn},
    health::{health_status_to_sql, provider_health_from_conn},
    parse_time_sql, CollectionRecord, Storage, StoredDailyUsage, StoredDailyUsageHistory,
    StoredForecastHistory, StoredUsageDashboard, StoredWindowObservation,
    FORECAST_OBSERVATIONS_QUERY, MAX_SNAPSHOTS_PER_ACCOUNT, SNAPSHOT_RETENTION_DAYS,
    UPSERT_DAILY_USAGE_QUERY,
};

impl Storage {
    #[cfg(test)]
    pub async fn upsert_local_usage_overlay(
        &self,
        account_id: &AccountId,
        dataset: &UsageDataset,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !dataset.authoritative,
            "local usage overlays must be supplemental"
        );
        anyhow::ensure!(
            !dataset.source_id.trim().is_empty(),
            "local usage overlays require a stable source id"
        );
        anyhow::ensure!(
            dataset.collection.daily_usage.is_empty(),
            "local usage overlays do not yet support daily usage buckets"
        );
        let account_id = account_id.clone();
        let dataset = dataset.clone();
        self.with_connection(move |conn| {
            let provider_id = &dataset.collection.usage.provider_id;
            ensure_overlay_account(conn, &account_id, provider_id)?;
            upsert_local_usage_overlay_conn(
                conn,
                &account_id,
                &dataset,
                dataset.collection.usage.collected_at,
            )?;
            Ok(())
        })
        .await
    }

    pub async fn reconcile_local_usage_overlays(
        &self,
        account_id: &AccountId,
        provider_id: &ProviderId,
        datasets: &[UsageDataset],
        reconciliation_started_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let account_id = account_id.clone();
        let provider_id = provider_id.clone();
        let datasets = datasets.to_vec();
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            ensure_overlay_account(&transaction, &account_id, &provider_id)?;

            let mut sources = BTreeSet::new();
            for dataset in &datasets {
                validate_local_usage_overlay(dataset, &provider_id)?;
                anyhow::ensure!(
                    sources.insert(dataset.source_key().to_string()),
                    "local usage overlays contain a duplicate source id"
                );
                upsert_local_usage_overlay_conn(
                    &transaction,
                    &account_id,
                    dataset,
                    reconciliation_started_at,
                )?;
            }

            let version =
                reconciliation_started_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            let stale_sources = {
                let mut stmt = transaction.prepare(
                    "SELECT source FROM local_usage_overlays
                     WHERE provider_id = ?1 AND account_id = ?2 AND collected_at <= ?3",
                )?;
                let rows = stmt.query_map(
                    params![provider_id.as_str(), account_id.as_str(), version],
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for source in stale_sources {
                if !sources.contains(&source) {
                    transaction.execute(
                        "DELETE FROM local_usage_overlays
                         WHERE provider_id = ?1 AND account_id = ?2 AND source = ?3
                           AND collected_at <= ?4",
                        params![provider_id.as_str(), account_id.as_str(), source, version],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

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

    pub async fn record_collection(&self, record: CollectionRecord<'_>) -> anyhow::Result<()> {
        let CollectionRecord {
            snapshot,
            daily_usage,
            usage_events,
            health,
            email,
            backoff,
            clear_backoff,
        } = record;
        let snapshot = snapshot.clone();
        let daily_usage = daily_usage.to_vec();
        let usage_events = usage_events.cloned();
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

            if let Some(batch) = usage_events.as_ref() {
                transaction.execute(
                    "DELETE FROM provider_daily_usage
                     WHERE provider_id = ?1 AND account_id = ?2
                       AND source = ?3
                       AND usage_date >= ?4 AND usage_date <= ?5",
                    params![
                        snapshot.provider_id.as_str(),
                        snapshot.account_id.as_str(),
                        batch.daily_source.as_str(),
                        batch.period_start.with_timezone(&Local).date_naive().to_string(),
                        batch.period_end.with_timezone(&Local).date_naive().to_string(),
                    ],
                )?;
            }
            upsert_daily_usage_buckets(
                &transaction,
                &snapshot.provider_id,
                &snapshot.account_id,
                daily_usage,
                &collected_at,
            )?;
            if let Some(batch) = usage_events {
                replace_usage_events(
                    &transaction,
                    &snapshot.provider_id,
                    &snapshot.account_id,
                    batch,
                    &collected_at,
                )?;
            }

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

    pub async fn latest_usage(&self) -> anyhow::Result<Vec<UsageSnapshot>> {
        self.with_connection(latest_usage_from_conn).await
    }

    pub async fn usage_events(
        &self,
        account_id: &AccountId,
        offset: u32,
        limit: u16,
    ) -> anyhow::Result<UsageEventPage> {
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            let total_count = conn.query_row(
                "SELECT COUNT(*) FROM provider_usage_events WHERE account_id = ?1",
                params![account_id.as_str()],
                |row| row.get::<_, i64>(0),
            )?;
            let total_count = u64::try_from(total_count)?;
            let mut statement = conn.prepare(
                "SELECT normalized_json FROM provider_usage_events
                 WHERE account_id = ?1
                 ORDER BY occurred_at DESC, event_id DESC
                 LIMIT ?2 OFFSET ?3",
            )?;
            let events = statement
                .query_map(
                    params![account_id.as_str(), i64::from(limit), i64::from(offset),],
                    |row| row.get::<_, String>(0),
                )?
                .map(|row| Ok(serde_json::from_str::<UsageEvent>(&row?)?))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let returned = u32::try_from(events.len())?;
            let next = offset
                .checked_add(returned)
                .filter(|next| u64::from(*next) < total_count);
            Ok(UsageEventPage {
                account_id,
                events,
                offset,
                total_count,
                next_offset: next,
            })
        })
        .await
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

fn replace_usage_events(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: &AccountId,
    batch: ProviderUsageEventBatch,
    collected_at: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        batch.period_start <= batch.period_end,
        "usage event replacement period was invalid"
    );
    conn.execute(
        "DELETE FROM provider_usage_events
         WHERE provider_id = ?1 AND account_id = ?2
           AND occurred_at >= ?3 AND occurred_at <= ?4",
        params![
            provider_id.as_str(),
            account_id.as_str(),
            batch.period_start.to_rfc3339(),
            batch.period_end.to_rfc3339(),
        ],
    )?;
    let mut statement = conn.prepare_cached(
        "INSERT INTO provider_usage_events
         (provider_id, account_id, event_id, occurred_at, collected_at, normalized_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for event in batch.events {
        anyhow::ensure!(
            event.occurred_at >= batch.period_start && event.occurred_at <= batch.period_end,
            "usage event fell outside its replacement period"
        );
        statement.execute(params![
            provider_id.as_str(),
            account_id.as_str(),
            event.event_id,
            event.occurred_at.to_rfc3339(),
            collected_at,
            serde_json::to_string(&event)?,
        ])?;
    }
    Ok(())
}

fn validate_local_usage_overlay(
    dataset: &UsageDataset,
    provider_id: &ProviderId,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !dataset.authoritative,
        "local usage overlays must be supplemental"
    );
    anyhow::ensure!(
        dataset.collection.usage.provider_id == *provider_id,
        "local usage overlay belongs to a different provider"
    );
    anyhow::ensure!(
        !dataset.source_id.trim().is_empty(),
        "local usage overlays require a stable source id"
    );
    anyhow::ensure!(
        dataset.collection.daily_usage.is_empty(),
        "local usage overlays do not yet support daily usage buckets"
    );
    Ok(())
}

fn ensure_overlay_account(
    conn: &Connection,
    account_id: &AccountId,
    provider_id: &ProviderId,
) -> anyhow::Result<()> {
    let belongs_to_account: bool = conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM accounts WHERE id = ?1 AND provider_id = ?2
         )",
        params![account_id.as_str(), provider_id.as_str()],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        belongs_to_account,
        "local usage overlay account does not belong to its provider"
    );
    Ok(())
}

fn upsert_local_usage_overlay_conn(
    conn: &Connection,
    account_id: &AccountId,
    dataset: &UsageDataset,
    version: DateTime<Utc>,
) -> anyhow::Result<()> {
    let provider_id = &dataset.collection.usage.provider_id;
    conn.execute(
        "INSERT INTO local_usage_overlays
         (provider_id, account_id, source, collected_at, dataset_json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider_id, account_id, source) DO UPDATE SET
           collected_at = excluded.collected_at,
           dataset_json = excluded.dataset_json
         WHERE excluded.collected_at >= local_usage_overlays.collected_at",
        params![
            provider_id.as_str(),
            account_id.as_str(),
            dataset.source_key(),
            version.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            serde_json::to_string(dataset)?,
        ],
    )?;
    Ok(())
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
    let mut snapshots = stmt
        .query_map([], usage_snapshot_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let overlays = local_usage_overlays_from_conn(conn)?;
    for snapshot in &mut snapshots {
        if let Some(datasets) = overlays.get(&(
            snapshot.provider_id.as_str().to_string(),
            snapshot.account_id.as_str().to_string(),
        )) {
            apply_local_usage_overlays(snapshot, datasets);
        }
    }
    Ok(snapshots)
}

fn local_usage_overlays_from_conn(
    conn: &Connection,
) -> anyhow::Result<HashMap<(String, String), Vec<UsageDataset>>> {
    let mut stmt = conn.prepare(
        "SELECT provider_id, account_id, dataset_json
         FROM local_usage_overlays
         ORDER BY provider_id, account_id, source",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut overlays = HashMap::<(String, String), Vec<UsageDataset>>::new();
    for (provider_id, account_id, json) in rows {
        overlays
            .entry((provider_id, account_id))
            .or_default()
            .push(serde_json::from_str(&json)?);
    }
    Ok(overlays)
}

fn apply_local_usage_overlays(snapshot: &mut UsageSnapshot, overlays: &[UsageDataset]) {
    let mut provenance = snapshot
        .metadata
        .get("dataset_provenance")
        .and_then(|value| Vec::<DatasetProvenance>::deserialize(value).ok())
        .unwrap_or_default();

    for overlay in overlays {
        let existing_for_source = provenance
            .iter()
            .any(|dataset| local_provenance_matches(dataset, overlay));
        if existing_for_source && snapshot.collected_at >= overlay.collection.usage.collected_at {
            continue;
        }

        let mut replaced_window_ids = std::collections::BTreeSet::new();
        let mut replaced_metadata_keys = std::collections::BTreeSet::new();
        provenance.retain(|dataset| {
            let replaced = local_provenance_matches(dataset, overlay);
            if replaced {
                replaced_window_ids.extend(dataset.window_ids.iter().cloned());
                replaced_metadata_keys.extend(dataset.metadata_keys.iter().cloned());
            }
            !replaced
        });
        snapshot
            .windows
            .retain(|window| !replaced_window_ids.contains(&window.window_id));
        let mut window_ids = snapshot
            .windows
            .iter()
            .map(|window| window.window_id.clone())
            .collect::<BTreeSet<_>>();

        if !snapshot.metadata.is_object() {
            snapshot.metadata = serde_json::json!({});
        }
        let metadata = snapshot
            .metadata
            .as_object_mut()
            .expect("metadata was normalized to an object");
        for key in replaced_metadata_keys {
            metadata.remove(&key);
        }
        let claimed_metadata_keys = provenance
            .iter()
            .flat_map(|dataset| dataset.metadata_keys.iter().cloned())
            .collect::<BTreeSet<_>>();
        if !existing_for_source
            && overlay.provenance.source == usage_core::UsageDataSource::LocalLogs
        {
            if let Some(incoming) = overlay.collection.usage.metadata.as_object() {
                for key in incoming.keys() {
                    if !claimed_metadata_keys.contains(key) {
                        metadata.remove(key);
                    }
                }
            }
        }
        let mut contributed_metadata_keys = Vec::new();
        if let Some(incoming) = overlay.collection.usage.metadata.as_object() {
            for (key, value) in incoming {
                if let serde_json::map::Entry::Vacant(entry) = metadata.entry(key.clone()) {
                    contributed_metadata_keys.push(key.clone());
                    entry.insert(value.clone());
                }
            }
        }
        let mut contributed_window_ids = Vec::new();
        for window in &overlay.collection.usage.windows {
            if window_ids.insert(window.window_id.clone()) {
                contributed_window_ids.push(window.window_id.clone());
                snapshot.windows.push(window.clone());
            }
        }
        snapshot.collected_at = snapshot
            .collected_at
            .max(overlay.collection.usage.collected_at);
        let mut provenance_record = overlay.provenance_record();
        provenance_record.window_ids = contributed_window_ids;
        provenance_record.metadata_keys = contributed_metadata_keys;
        provenance.push(provenance_record);
    }

    if let Some(metadata) = snapshot.metadata.as_object_mut() {
        metadata.insert(
            "dataset_provenance".to_string(),
            serde_json::to_value(provenance).expect("dataset provenance is serializable"),
        );
    }
}

fn local_provenance_matches(dataset: &DatasetProvenance, overlay: &UsageDataset) -> bool {
    if dataset.authoritative {
        return false;
    }
    if !dataset.source_id.is_empty() && !overlay.source_id.is_empty() {
        dataset.source_id == overlay.source_id
    } else {
        dataset.provenance.source == overlay.provenance.source
    }
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
    conn.execute(
        "DELETE FROM provider_usage_events
         WHERE provider_id = ?1 AND account_id = ?2 AND occurred_at < ?3",
        params![
            provider_id.as_str(),
            account_id.as_str(),
            retention_cutoff.to_rfc3339(),
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
