use std::path::Path;

use chrono::{DateTime, Days, TimeDelta, Utc};
use clap::ValueEnum;
use usage_core::{
    Account, AccountId, ProviderHealth, ProviderHealthStatus, ProviderId, UsageAmount,
    UsageSnapshot, UsageUnit, UsageWindow, UsageWindowKind,
};

use crate::{notifications::NotificationManager, providers::DailyUsageBucket, storage::Storage};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum FixtureScenario {
    /// Accounts, quota windows, history, costs, forecasts, and provider errors.
    Demo,
    /// The demo dataset with several low and exhausted windows queued as alerts.
    Notifications,
}

impl FixtureScenario {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Demo => "demo",
            Self::Notifications => "notifications",
        }
    }
}

pub fn reset_database(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub async fn seed(storage: &Storage, scenario: FixtureScenario) -> anyhow::Result<()> {
    let now = Utc::now();
    let accounts = [
        FixtureAccount::new("codex", "personal", "Personal", "alex@example.test", 62.0),
        FixtureAccount::new("codex", "work", "Work", "alex@work.example", 8.0),
        FixtureAccount::new("claude", "team", "Team", "alex@team.example", 71.0),
        FixtureAccount::new(
            "opencode_go",
            "sandbox",
            "Sandbox",
            "alex@sandbox.example",
            46.0,
        )
        .with_health(
            ProviderHealthStatus::RateLimited,
            "Fixture: retry available in 8 minutes",
        ),
    ];

    let notification_manager = NotificationManager::new(storage.clone(), true);
    for (index, fixture) in accounts.iter().enumerate() {
        let account = storage
            .upsert_account(
                &ProviderId::new(fixture.provider_id),
                &format!("fixture-{}-{}", fixture.provider_id, fixture.profile_id),
                Some(fixture.profile_id),
                Some(fixture.display_name),
                Some(fixture.email),
            )
            .await?;
        let latest = seed_account(storage, &account, fixture, scenario, now, index).await?;
        if scenario == FixtureScenario::Notifications {
            notification_manager
                .process_snapshot(&account, &latest)
                .await;
        }
    }
    Ok(())
}

async fn seed_account(
    storage: &Storage,
    account: &Account,
    fixture: &FixtureAccount,
    scenario: FixtureScenario,
    now: DateTime<Utc>,
    account_index: usize,
) -> anyhow::Result<UsageSnapshot> {
    let latest_remaining = match scenario {
        FixtureScenario::Demo => fixture.percent_remaining,
        FixtureScenario::Notifications => [4.0, 0.0, 9.0, 5.0][account_index],
    };
    let session_reset = now + TimeDelta::hours(3);
    let weekly_reset = now + TimeDelta::days(5);
    let daily_usage = daily_usage(fixture.provider_id, account_index, now);
    let metadata = fixture_metadata(fixture.provider_id, &daily_usage, now);
    let mut latest = None;

    for sample in 0..6 {
        let hours_ago = i64::from(5 - sample) * 2;
        let collected_at = now - TimeDelta::hours(hours_ago) - TimeDelta::minutes(2);
        let historical_remaining = (latest_remaining + f64::from(5 - sample) * 3.5).min(100.0);
        let snapshot = UsageSnapshot {
            provider_id: account.provider_id.clone(),
            account_id: account.id.clone(),
            collected_at,
            windows: fixture_windows(
                fixture.provider_id,
                historical_remaining,
                session_reset,
                weekly_reset,
            ),
            metadata: metadata.clone(),
        };
        let health = fixture_health(fixture, &account.id, collected_at);
        let buckets = if sample == 5 {
            daily_usage.as_slice()
        } else {
            &[]
        };
        storage
            .record_success(&snapshot, buckets, &health, Some(fixture.email))
            .await?;
        latest = Some(snapshot);
    }

    latest.ok_or_else(|| anyhow::anyhow!("fixture did not create a snapshot"))
}

fn fixture_windows(
    provider_id: &str,
    percent_remaining: f64,
    session_reset: DateTime<Utc>,
    weekly_reset: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let session_remaining = percent_remaining.clamp(0.0, 100.0);
    let weekly_remaining = (percent_remaining + 17.0).clamp(0.0, 100.0);
    let mut windows = vec![
        quota_window(
            "session",
            "Session",
            UsageWindowKind::Session,
            session_remaining,
            session_reset,
        ),
        quota_window(
            "weekly",
            "Weekly",
            UsageWindowKind::Weekly,
            weekly_remaining,
            weekly_reset,
        ),
    ];
    if provider_id == "opencode_go" {
        windows.push(UsageWindow {
            window_id: "credits".to_string(),
            label: "Zen credits".to_string(),
            kind: UsageWindowKind::Credits,
            used: Some(amount(13.75, UsageUnit::Credits)),
            limit: Some(amount(25.0, UsageUnit::Credits)),
            remaining: Some(amount(11.25, UsageUnit::Credits)),
            percent_used: Some(55.0),
            percent_remaining: Some(45.0),
            reset_at: None,
        });
    }
    windows
}

fn quota_window(
    id: &str,
    label: &str,
    kind: UsageWindowKind,
    percent_remaining: f64,
    reset_at: DateTime<Utc>,
) -> UsageWindow {
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind,
        used: Some(amount(100.0 - percent_remaining, UsageUnit::Percent)),
        limit: Some(amount(100.0, UsageUnit::Percent)),
        remaining: Some(amount(percent_remaining, UsageUnit::Percent)),
        percent_used: Some(100.0 - percent_remaining),
        percent_remaining: Some(percent_remaining),
        reset_at: Some(reset_at),
    }
}

fn amount(value: f64, unit: UsageUnit) -> UsageAmount {
    UsageAmount { value, unit }
}

fn daily_usage(
    provider_id: &str,
    account_index: usize,
    now: DateTime<Utc>,
) -> Vec<DailyUsageBucket> {
    let today = now.date_naive();
    (0_u64..30)
        .map(|days_ago| {
            let date = today
                .checked_sub_days(Days::new(29 - days_ago))
                .unwrap_or(today);
            let wave = ((days_ago * 37 + account_index as u64 * 19) % 11) + 2;
            let tokens = wave * 115_000 * (account_index as u64 + 1);
            let rate = match provider_id {
                "claude" => 0.000_004_5,
                "opencode_go" => 0.000_001_2,
                _ => 0.000_003,
            };
            DailyUsageBucket {
                date,
                tokens,
                cost_usd: Some(tokens as f64 * rate),
                source: "development_fixture".to_string(),
            }
        })
        .collect()
}

fn fixture_metadata(
    provider_id: &str,
    daily_usage: &[DailyUsageBucket],
    now: DateTime<Utc>,
) -> serde_json::Value {
    let rows = daily_usage
        .iter()
        .map(|bucket| {
            serde_json::json!({
                "date": bucket.date.to_string(),
                "tokens": bucket.tokens,
                "priced_tokens": bucket.tokens,
                "cost_usd": bucket.cost_usd,
            })
        })
        .collect::<Vec<_>>();
    let mut metadata = serde_json::json!({
        format!("{provider_id}_cost"): {
            "source": "development_fixture",
            "estimate": true,
            "partial": false,
            "complete_lookback": true,
            "by_day": rows,
        },
        "fixture": true,
    });
    // Codex is the only provider that exposes rate-limit reset credits, so seed
    // a small pool here to exercise the "N resets" summary and Resets detail.
    if provider_id == "codex" {
        if let Some(object) = metadata.as_object_mut() {
            object.insert(
                "rate_limit_reset_credits".to_string(),
                fixture_reset_credits(now),
            );
        }
    }
    metadata
}

fn fixture_reset_credits(now: DateTime<Utc>) -> serde_json::Value {
    let credits = [
        ("Session reset", TimeDelta::hours(20)),
        ("Weekly reset", TimeDelta::days(2)),
        ("Weekly reset", TimeDelta::days(6)),
    ];
    let credit_rows = credits
        .iter()
        .enumerate()
        .map(|(index, (title, delta))| {
            serde_json::json!({
                "id": format!("fixture-reset-{index}"),
                "title": title,
                "status": "available",
                "expires_at": (now + *delta).timestamp(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "available_count": credit_rows.len(),
        "next_expires_at": (now + credits[0].1).timestamp(),
        "credits": credit_rows,
    })
}

fn fixture_health(
    fixture: &FixtureAccount,
    account_id: &AccountId,
    collected_at: DateTime<Utc>,
) -> ProviderHealth {
    let is_ok = matches!(fixture.health, ProviderHealthStatus::Ok);
    ProviderHealth {
        provider_id: ProviderId::new(fixture.provider_id),
        account_id: Some(account_id.clone()),
        status: fixture.health.clone(),
        collection_mode: Some("development_fixture".to_string()),
        last_success_at: Some(collected_at),
        last_failure_at: (!is_ok).then_some(collected_at),
        last_error_code: (!is_ok).then(|| "fixture_rate_limit".to_string()),
        last_error_message: fixture.error_message.map(str::to_string),
        updated_at: collected_at,
    }
}

struct FixtureAccount {
    provider_id: &'static str,
    profile_id: &'static str,
    display_name: &'static str,
    email: &'static str,
    percent_remaining: f64,
    health: ProviderHealthStatus,
    error_message: Option<&'static str>,
}

impl FixtureAccount {
    fn new(
        provider_id: &'static str,
        profile_id: &'static str,
        display_name: &'static str,
        email: &'static str,
        percent_remaining: f64,
    ) -> Self {
        Self {
            provider_id,
            profile_id,
            display_name,
            email,
            percent_remaining,
            health: ProviderHealthStatus::Ok,
            error_message: None,
        }
    }

    fn with_health(mut self, health: ProviderHealthStatus, message: &'static str) -> Self {
        self.health = health;
        self.error_message = Some(message);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notification_fixture_populates_real_storage_models() {
        let root = std::env::temp_dir().join(format!("usage-fixture-{}", uuid::Uuid::new_v4()));
        let db_path = root.join("usage.sqlite3");
        let storage = Storage::open(&db_path).unwrap();

        seed(&storage, FixtureScenario::Notifications)
            .await
            .unwrap();

        assert_eq!(storage.accounts().await.unwrap().len(), 4);
        assert_eq!(storage.latest_usage().await.unwrap().len(), 4);
        assert_eq!(storage.provider_health().await.unwrap().len(), 4);
        assert_eq!(storage.daily_usage_history().await.unwrap().len(), 120);
        let notifications = storage.pending_notifications().await.unwrap();
        assert!(notifications.len() >= 4);
        assert!(notifications
            .iter()
            .any(|item| item.title.contains("exhausted")));

        drop(storage);
        let _ = std::fs::remove_dir_all(root);
    }
}
