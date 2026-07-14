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
        FixtureAccount::new("codex", 4.0),
        FixtureAccount::additional(
            "codex",
            "work",
            "Acme Engineering",
            "joey@acme.example",
            -24.0,
            0.0,
        ),
        FixtureAccount::new("claude", 9.0),
        FixtureAccount::additional(
            "claude",
            "work",
            "Acme Research",
            "joey@research.example",
            -31.0,
            5.0,
        ),
        FixtureAccount::new("opencode_go", 7.0),
        FixtureAccount::new("grok", 3.0),
    ];

    let notification_manager = NotificationManager::new(storage.clone(), true);
    for (index, fixture) in accounts.iter().enumerate() {
        let account = storage
            .upsert_account(
                &ProviderId::new(fixture.provider_id),
                &format!("fixture-{}-{}", fixture.provider_id, fixture.profile_id),
                Some(fixture.profile_id),
                Some(fixture.display_name),
                fixture.email,
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
    let notification_remaining =
        (scenario == FixtureScenario::Notifications).then_some(fixture.notification_remaining);
    let daily_usage = daily_usage(fixture.provider_id, account_index, now);
    let cost_usage = cost_usage(fixture.provider_id, now);
    let metadata = fixture_metadata(fixture.provider_id, &cost_usage, now);
    let mut latest = None;

    for sample in 0..6 {
        let hours_ago = i64::from(5 - sample) * 2;
        let collected_at = now - TimeDelta::hours(hours_ago) - TimeDelta::minutes(2);
        let historical_boost = f64::from(5 - sample) * 3.5;
        let snapshot = UsageSnapshot {
            provider_id: account.provider_id.clone(),
            account_id: account.id.clone(),
            collected_at,
            windows: fixture_windows(
                fixture.provider_id,
                historical_boost,
                fixture.percent_adjustment,
                notification_remaining,
                now,
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
            .record_collection(&snapshot, buckets, &health, fixture.email, None, true)
            .await?;
        latest = Some(snapshot);
    }

    latest.ok_or_else(|| anyhow::anyhow!("fixture did not create a snapshot"))
}

fn fixture_windows(
    provider_id: &str,
    historical_boost: f64,
    percent_adjustment: f64,
    notification_remaining: Option<f64>,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let percent = |value: f64| {
        notification_remaining
            .unwrap_or(value + historical_boost + percent_adjustment)
            .clamp(0.0, 100.0)
    };
    match provider_id {
        "claude" => vec![
            quota_window(
                "claude_usage_utilization_five_hour",
                "Claude five hour",
                UsageWindowKind::Session,
                percent(68.0),
                None,
            ),
            quota_window(
                "claude_usage_utilization_seven_day",
                "Claude seven day",
                UsageWindowKind::Daily,
                percent(75.0),
                Some(now + TimeDelta::hours(42)),
            ),
            metric_window(
                "claude_estimated_spend_today",
                "Claude spend today",
                UsageWindowKind::Credits,
                2.4705995,
                UsageUnit::Usd,
            ),
            metric_window(
                "claude_tokens_today",
                "Claude tokens today",
                UsageWindowKind::Daily,
                1_828_162.0,
                UsageUnit::Tokens,
            ),
            metric_window(
                "claude_estimated_spend_30d",
                "Claude spend 30 days",
                UsageWindowKind::Credits,
                138.283568,
                UsageUnit::Usd,
            ),
            metric_window(
                "claude_tokens_30d",
                "Claude tokens 30 days",
                UsageWindowKind::Monthly,
                100_084_986.0,
                UsageUnit::Tokens,
            ),
        ],
        "codex" => vec![
            quota_window(
                "codex_session",
                "Codex session",
                UsageWindowKind::Session,
                percent(56.0),
                Some(now + TimeDelta::minutes(9_366)),
            ),
            quota_window(
                "codex_additional_0_session",
                "GPT-5.3-Codex-Spark session",
                UsageWindowKind::Session,
                percent(91.0),
                Some(now + TimeDelta::hours(168)),
            ),
            balance_window("codex_credits", "Codex credits", 0.0, UsageUnit::Credits),
            metric_window(
                "codex_tokens_today",
                "Codex tokens today",
                UsageWindowKind::Daily,
                125_177_016.0,
                UsageUnit::Tokens,
            ),
            metric_window(
                "codex_tokens_30d",
                "Codex tokens 30 days",
                UsageWindowKind::Monthly,
                1_092_768_692.0,
                UsageUnit::Tokens,
            ),
            metric_window(
                "codex_tokens_lifetime",
                "Codex lifetime tokens",
                UsageWindowKind::Other("lifetime".to_string()),
                2_373_310_905.0,
                UsageUnit::Tokens,
            ),
            metric_window(
                "codex_estimated_spend_today",
                "Codex estimated cost today",
                UsageWindowKind::Credits,
                129.57697885,
                UsageUnit::Usd,
            ),
            metric_window(
                "codex_estimated_spend_30d",
                "Codex estimated cost 30 days",
                UsageWindowKind::Credits,
                1_005.4623066,
                UsageUnit::Usd,
            ),
        ],
        "grok" => vec![quota_window(
            "grok_included_usage",
            "Grok weekly",
            UsageWindowKind::Weekly,
            percent(100.0),
            Some(now + TimeDelta::minutes(2_490)),
        )],
        "opencode_go" => vec![
            quota_window(
                "opencode_go_session",
                "OpenCode Go session",
                UsageWindowKind::Session,
                percent(72.0),
                Some(now + TimeDelta::hours(5)),
            ),
            quota_window(
                "opencode_go_weekly",
                "OpenCode Go weekly",
                UsageWindowKind::Weekly,
                percent(100.0),
                Some(now + TimeDelta::hours(161)),
            ),
            quota_window(
                "opencode_go_monthly",
                "OpenCode Go monthly",
                UsageWindowKind::Monthly,
                percent(89.0),
                Some(now + TimeDelta::hours(492)),
            ),
            balance_window(
                "opencode_go_zen_balance",
                "OpenCode Go Zen balance",
                0.0,
                UsageUnit::Credits,
            ),
            metric_window(
                "opencode_go_spend_today",
                "OpenCode Go spend today",
                UsageWindowKind::Credits,
                0.18463836,
                UsageUnit::Usd,
            ),
            metric_window(
                "opencode_go_tokens_today",
                "OpenCode Go tokens today",
                UsageWindowKind::Daily,
                250_682.0,
                UsageUnit::Tokens,
            ),
            metric_window(
                "opencode_go_spend_30d",
                "OpenCode Go spend 30 days",
                UsageWindowKind::Credits,
                7.05145503,
                UsageUnit::Usd,
            ),
            metric_window(
                "opencode_go_tokens_30d",
                "OpenCode Go tokens 30 days",
                UsageWindowKind::Monthly,
                18_583_277.0,
                UsageUnit::Tokens,
            ),
        ],
        _ => vec![],
    }
}

fn quota_window(
    id: &str,
    label: &str,
    kind: UsageWindowKind,
    percent_remaining: f64,
    reset_at: Option<DateTime<Utc>>,
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
        reset_at,
    }
}

fn metric_window(
    id: &str,
    label: &str,
    kind: UsageWindowKind,
    value: f64,
    unit: UsageUnit,
) -> UsageWindow {
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind,
        used: Some(amount(value, unit)),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

fn balance_window(id: &str, label: &str, value: f64, unit: UsageUnit) -> UsageWindow {
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: Some(amount(value, unit)),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

fn amount(value: f64, unit: UsageUnit) -> UsageAmount {
    UsageAmount { value, unit }
}

fn daily_usage(
    provider_id: &str,
    _account_index: usize,
    now: DateTime<Utc>,
) -> Vec<DailyUsageBucket> {
    let today = now.date_naive();
    let rows: &[(u64, u64)] = match provider_id {
        "codex" => &[
            (0, 125_177_016),
            (1, 294_085_085),
            (2, 200_266_476),
            (3, 165_189_394),
            (4, 48_166_440),
            (5, 30_876_970),
            (6, 5_851_953),
            (7, 13_967_530),
            (8, 4_560_589),
            (9, 37_105_778),
            (10, 34_949_981),
            (11, 1_519_493),
            (12, 952_034),
            (13, 154_162),
            (14, 3_874_492),
            (15, 3_350_238),
            (16, 3_268_115),
            (17, 758_269),
            (18, 9_080_755),
            (19, 11_303_581),
            (27, 16_641_805),
            (28, 40_848_936),
            (29, 40_819_600),
            (30, 46_333_178),
            (31, 63_247_540),
            (32, 42_400_525),
            (33, 53_293_941),
            (34, 36_735_277),
            (35, 27_748_690),
            (36, 21_373_376),
        ],
        _ => &[],
    };
    rows.iter()
        .map(|&(days_ago, tokens)| DailyUsageBucket {
            date: today.checked_sub_days(Days::new(days_ago)).unwrap_or(today),
            tokens,
            cost_usd: None,
            source: "development_fixture".to_string(),
        })
        .collect()
}

fn cost_usage(provider_id: &str, now: DateTime<Utc>) -> Vec<DailyUsageBucket> {
    let today = now.date_naive();
    let rows: &[(u64, u64, f64)] = match provider_id {
        "claude" => &[
            (28, 4_243_844, 3.859996),
            (9, 17_420_333, 24.413996),
            (8, 1_581_767, 2.101908),
            (5, 24_364_583, 49.79609875),
            (3, 18_825_024, 22.07300375),
            (2, 18_294_406, 20.3673225),
            (1, 13_526_867, 13.2006435),
            (0, 1_828_162, 2.4705995),
        ],
        "codex" => &[
            (29, 30_723_250, 33.524919),
            (28, 47_587_379, 48.725849),
            (27, 753_713, 0.477775),
            (20, 2_511_914, 3.072646),
            (19, 13_246_516, 14.687046),
            (18, 4_712_581, 4.581964),
            (17, 720_592, 1.197307),
            (15, 3_124_330, 4.506302),
            (14, 442_889, 0.607034),
            (12, 56_120, 0.235862),
            (11, 899_023, 1.764509),
            (10, 42_393_794, 42.320906),
            (9, 7_267_955, 9.590157),
            (6, 65_305, 0.164905),
            (5, 42_626_924, 47.158127),
            (4, 47_871_934, 48.439674),
            (3, 210_919_063, 177.637233),
            (2, 329_746_264, 253.2681166),
            (1, 242_521_874, 183.92499615),
            (0, 147_696_220, 129.86053985),
        ],
        "opencode_go" => &[
            (31, 1_064_164, 0.16532331),
            (30, 3_762_023, 0.86840012),
            (20, 53_233, 0.04686621),
            (17, 53_094, 0.05446812),
            (16, 34_118, 0.0300238),
            (15, 27_659, 0.04511284),
            (10, 523_464, 0.247344),
            (9, 423_608, 0.2545648),
            (5, 7_176_189, 3.10123818),
            (4, 1_560_493, 0.41136594),
            (3, 5_687_731, 1.25099482),
            (2, 2_357_220, 1.18971953),
            (1, 435_786, 0.23511843),
            (0, 250_682, 0.18463836),
        ],
        _ => &[],
    };
    rows.iter()
        .map(|&(days_ago, tokens, cost_usd)| {
            let date = today.checked_sub_days(Days::new(days_ago)).unwrap_or(today);
            DailyUsageBucket {
                date,
                tokens,
                cost_usd: Some(cost_usd),
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
    let mut metadata = serde_json::json!({ "fixture": true });
    if !rows.is_empty() {
        metadata.as_object_mut().unwrap().insert(
            format!("{provider_id}_cost"),
            serde_json::json!({
                "source": "development_fixture",
                "estimate": true,
                "partial": false,
                "complete_lookback": true,
                "by_day": rows,
            }),
        );
    }
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
    email: Option<&'static str>,
    health: ProviderHealthStatus,
    error_message: Option<&'static str>,
    percent_adjustment: f64,
    notification_remaining: f64,
}

impl FixtureAccount {
    fn new(provider_id: &'static str, notification_remaining: f64) -> Self {
        Self {
            provider_id,
            profile_id: "joeymalvinni",
            display_name: "joeymalvinni",
            email: None,
            health: ProviderHealthStatus::Ok,
            error_message: None,
            percent_adjustment: 0.0,
            notification_remaining,
        }
    }

    fn additional(
        provider_id: &'static str,
        profile_id: &'static str,
        display_name: &'static str,
        email: &'static str,
        percent_adjustment: f64,
        notification_remaining: f64,
    ) -> Self {
        Self {
            provider_id,
            profile_id,
            display_name,
            email: Some(email),
            health: ProviderHealthStatus::Ok,
            error_message: None,
            percent_adjustment,
            notification_remaining,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_fixture_gives_providers_and_accounts_distinct_session_limits() {
        let now = Utc::now();
        let expected = [
            ("claude", "claude_usage_utilization_five_hour", 68.0),
            ("codex", "codex_session", 56.0),
            ("codex", "codex_additional_0_session", 91.0),
            ("opencode_go", "opencode_go_session", 72.0),
        ];

        for (provider_id, window_id, percent_remaining) in expected {
            let windows = fixture_windows(provider_id, 0.0, 0.0, None, now);
            let window = windows
                .iter()
                .find(|window| window.window_id == window_id)
                .unwrap();
            assert_eq!(window.percent_remaining, Some(percent_remaining));
        }

        for (provider_id, window_id, adjustment, percent_remaining) in [
            ("claude", "claude_usage_utilization_five_hour", -31.0, 37.0),
            ("codex", "codex_session", -24.0, 32.0),
        ] {
            let windows = fixture_windows(provider_id, 0.0, adjustment, None, now);
            let window = windows
                .iter()
                .find(|window| window.window_id == window_id)
                .unwrap();
            assert_eq!(window.percent_remaining, Some(percent_remaining));
        }
    }

    #[tokio::test]
    async fn notification_fixture_populates_real_storage_models() {
        let root = std::env::temp_dir().join(format!("usage-fixture-{}", uuid::Uuid::new_v4()));
        let db_path = root.join("usage.sqlite3");
        let storage = Storage::open(&db_path).unwrap();

        seed(&storage, FixtureScenario::Notifications)
            .await
            .unwrap();

        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 6);
        for provider_id in ["claude", "codex"] {
            let provider_accounts = accounts
                .iter()
                .filter(|account| account.provider_id.as_str() == provider_id)
                .collect::<Vec<_>>();
            assert_eq!(provider_accounts.len(), 2);
            assert!(provider_accounts.iter().any(|account| {
                account.profile_id.as_deref() == Some("joeymalvinni")
                    && account.display_name.as_deref() == Some("joeymalvinni")
                    && account.email.is_none()
            }));
            assert!(provider_accounts.iter().any(|account| {
                account.profile_id.as_deref() == Some("work") && account.email.is_some()
            }));
        }
        assert_eq!(storage.latest_usage().await.unwrap().len(), 6);
        assert_eq!(storage.provider_health().await.unwrap().len(), 6);
        assert_eq!(storage.daily_usage_history().await.unwrap().len(), 60);
        let notifications = storage.pending_notifications().await.unwrap();
        assert!(notifications.len() >= 6);
        assert!(notifications
            .iter()
            .any(|item| item.title.contains("exhausted")));

        drop(storage);
        let _ = std::fs::remove_dir_all(root);
    }
}
