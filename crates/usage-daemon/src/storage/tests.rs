use super::*;
use chrono::TimeZone;
use rusqlite::params;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use usage_core::{
    AccountDisplayNameSource, ProviderHealth, ProviderHealthStatus, UsageAmount, UsageUnit,
    UsageWindow, UsageWindowKind,
};
use uuid::Uuid;

use crate::providers::DailyUsageBucket;

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

#[tokio::test]
async fn usage_dashboard_reads_bounded_compact_forecast_history() {
    let storage = test_storage();
    let provider_id = ProviderId::new("codex");
    let account = storage
        .upsert_account(&provider_id, "external-account", None, None, None)
        .await
        .unwrap();
    let start = Utc.with_ymd_and_hms(2026, 7, 10, 10, 0, 0).unwrap();
    let reset_at = start + chrono::TimeDelta::hours(5);
    for (offset, percent) in [(0, 10.0), (5, 20.0), (10, 30.0)] {
        storage
            .insert_snapshot(
                &UsageSnapshot {
                    provider_id: provider_id.clone(),
                    account_id: account.id.clone(),
                    collected_at: start + chrono::TimeDelta::minutes(offset),
                    windows: vec![
                        percentage_window(
                            "codex_session",
                            UsageWindowKind::Session,
                            percent,
                            Some(reset_at),
                        ),
                        percentage_window("codex_tokens", UsageWindowKind::Tokens, percent, None),
                    ],
                    metadata: json!({"large": "metadata is already in normalized_json"}),
                },
                None,
            )
            .await
            .unwrap();
    }

    let dashboard = storage
        .usage_dashboard(start.date_naive(), start - chrono::TimeDelta::minutes(1), 2)
        .await
        .unwrap();

    assert_eq!(dashboard.snapshots.len(), 1);
    assert_eq!(
        dashboard.snapshots[0].collected_at,
        start + chrono::TimeDelta::minutes(10)
    );
    let history = dashboard
        .forecast_histories
        .get(&(provider_id, account.id))
        .unwrap();
    let observations = history.by_window.get("codex_session").unwrap();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].percent_used, 30.0);
    assert_eq!(observations[1].percent_used, 20.0);
    assert!(!history.by_window.contains_key("codex_tokens"));

    let (observation_count, uses_covering_index) = storage
        .with_connection(|conn| {
            let explain = format!("EXPLAIN QUERY PLAN {FORECAST_OBSERVATIONS_QUERY}");
            let mut plan = conn.prepare(&explain)?;
            let plan = plan
                .query_map(
                    params!["codex", "account", "window", "0", "9", 1_024_i64],
                    |row| row.get::<_, String>(3),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                conn.query_row(
                    "SELECT COUNT(*) FROM usage_window_observations",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                plan.iter().any(|detail| {
                    detail.contains("COVERING INDEX usage_window_observations_lookup")
                }),
            ))
        })
        .await
        .unwrap();
    assert_eq!(observation_count, 6);
    assert!(uses_covering_index);
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
async fn unchanged_daily_usage_does_not_rewrite_the_row() {
    let storage = test_storage();
    let provider_id = ProviderId::new("codex");
    let account = storage
        .upsert_account(&provider_id, "external-account", None, None, None)
        .await
        .unwrap();
    let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
    let first_collected_at = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
    let later_collected_at = first_collected_at + chrono::TimeDelta::hours(1);
    let bucket = |tokens| DailyUsageBucket {
        date,
        tokens,
        cost_usd: Some(1.25),
        source: "codex_account_usage".to_string(),
    };

    storage
        .upsert_daily_usage(&provider_id, &account.id, &[bucket(10)], first_collected_at)
        .await
        .unwrap();
    storage
        .upsert_daily_usage(&provider_id, &account.id, &[bucket(10)], later_collected_at)
        .await
        .unwrap();

    let account_id = account.id.clone();
    let unchanged_collected_at = storage
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT collected_at FROM provider_daily_usage
                     WHERE provider_id = 'codex' AND account_id = ?1 AND usage_date = '2026-07-09'",
                params![account_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(unchanged_collected_at, first_collected_at.to_rfc3339());

    storage
        .upsert_daily_usage(&provider_id, &account.id, &[bucket(11)], later_collected_at)
        .await
        .unwrap();
    let account_id = account.id.clone();
    let changed_collected_at = storage
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT collected_at FROM provider_daily_usage
                     WHERE provider_id = 'codex' AND account_id = ?1 AND usage_date = '2026-07-09'",
                params![account_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(changed_collected_at, later_collected_at.to_rfc3339());
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
                windows: vec![percentage_window(
                    "codex_session",
                    UsageWindowKind::Session,
                    25.0,
                    None,
                )],
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
    let observations = storage
        .with_connection(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM usage_window_observations",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(observations, 0);
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
async fn grok_default_profile_adopts_provider_identity_without_duplication() {
    let storage = test_storage();
    let provider_id = ProviderId::new("grok");
    let provisional = storage
        .upsert_account(&provider_id, "grok_default", Some("default"), None, None)
        .await
        .unwrap();

    let identified = storage
        .upsert_account(
            &provider_id,
            "grok-user-123",
            Some("default"),
            None,
            Some("user@example.com"),
        )
        .await
        .unwrap();

    assert_eq!(identified.id, provisional.id);
    assert_eq!(identified.external_account_id, "grok-user-123");
    assert_eq!(identified.email.as_deref(), Some("user@example.com"));
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
                    windows: vec![percentage_window(
                        "codex_session",
                        UsageWindowKind::Session,
                        f64::from(version),
                        None,
                    )],
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
    let (snapshots, raw_payloads, observations) = storage
        .with_connection(move |conn| {
            let snapshots: i64 = conn.query_row(
                "SELECT COUNT(*) FROM usage_snapshots WHERE account_id = ?1",
                params![account_id.as_str()],
                |row| row.get(0),
            )?;
            let raw_payloads: i64 =
                conn.query_row("SELECT COUNT(*) FROM raw_payloads", [], |row| row.get(0))?;
            let observations: i64 = conn.query_row(
                "SELECT COUNT(*) FROM usage_window_observations WHERE account_id = ?1",
                params![account_id.as_str()],
                |row| row.get(0),
            )?;
            Ok((snapshots, raw_payloads, observations))
        })
        .await
        .unwrap();
    assert_eq!(snapshots, 2);
    assert_eq!(raw_payloads, 1);
    assert_eq!(observations, 2);
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

fn test_storage() -> Storage {
    let path = std::env::temp_dir().join(format!("usage-storage-{}.sqlite3", Uuid::new_v4()));
    let storage = Storage::open(&path).unwrap();
    let _ = std::fs::remove_file(path);
    storage
}

fn percentage_window(
    window_id: &str,
    kind: UsageWindowKind,
    percent_used: f64,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: window_id.to_string(),
        kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used: Some(percent_used),
        percent_remaining: Some(100.0 - percent_used),
        reset_at,
    }
}
