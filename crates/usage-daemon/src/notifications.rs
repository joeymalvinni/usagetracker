use std::sync::{Arc, RwLock};

use chrono::{DateTime, Timelike, Utc};
use tracing::{debug, warn};
use usage_core::{
    Account, ForecastConfidence, ForecastStatus, NotificationConfig, NotificationQuietHours,
    UsageForecast, UsageSnapshot, UsageWindow,
};

use crate::storage::{NotificationWindowState, Storage};

const REARM_HYSTERESIS: f64 = 1.0;
const PREDICTIVE_NOTIFICATION_BIT: u8 = 1 << 7;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopNotification {
    pub title: String,
    pub body: String,
}

pub struct NotificationManager {
    storage: Storage,
    config: RwLock<NotificationConfig>,
}

impl NotificationManager {
    pub fn new(storage: Storage, config: impl Into<NotificationConfig>) -> Arc<Self> {
        Arc::new(Self {
            storage,
            config: RwLock::new(config.into()),
        })
    }

    pub fn enabled(&self) -> bool {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .enabled
    }

    pub fn set_config(&self, config: NotificationConfig) {
        *self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = config;
    }

    pub async fn process_snapshot(&self, account: &Account, snapshot: &UsageSnapshot) {
        self.process_snapshot_with_forecasts(account, snapshot, &[])
            .await;
    }

    pub async fn process_snapshot_with_forecasts(
        &self,
        account: &Account,
        snapshot: &UsageSnapshot,
        forecasts: &[UsageForecast],
    ) {
        if !self.enabled() {
            return;
        }
        let config = self
            .config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for window in &snapshot.windows {
            let policy = ResolvedNotificationPolicy::for_window(&config, account, window);
            let forecast = forecasts.iter().find(|forecast| {
                forecast.provider_id == snapshot.provider_id
                    && forecast.account_id == account.id
                    && forecast.window_id == window.window_id
            });
            if let Err(err) = self
                .process_window(account, snapshot, window, forecast, &policy)
                .await
            {
                warn!(
                    provider_id = snapshot.provider_id.as_str(),
                    account_id = account.id.as_str(),
                    window_id = window.window_id,
                    error = %err,
                    "usage notification evaluation failed"
                );
            }
        }
    }

    async fn process_window(
        &self,
        account: &Account,
        snapshot: &UsageSnapshot,
        window: &UsageWindow,
        forecast: Option<&UsageForecast>,
        policy: &ResolvedNotificationPolicy,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        if !policy.enabled
            || policy.snoozed_until.is_some_and(|deadline| deadline > now)
            || policy.quiet_hours.as_ref().is_some_and(is_quiet_now)
        {
            return Ok(());
        }
        if !snapshot.window_is_authoritative_quota(window) {
            debug!(
                provider_id = snapshot.provider_id.as_str(),
                account_id = account.id.as_str(),
                window_id = window.window_id,
                "skipping notifications for a non-authoritative usage window"
            );
            return Ok(());
        }
        let Some(percent) = window.percent_remaining.filter(|value| value.is_finite()) else {
            return Ok(());
        };
        let percent = percent.clamp(0.0, 100.0);
        let existing = self
            .storage
            .notification_window_state(&account.id, &window.window_id)
            .await?;
        let mut state = existing.clone().unwrap_or(NotificationWindowState {
            reset_at: window.reset_at,
            notified_mask: 0,
            last_attempt_at: None,
        });

        let reset_completed = existing.as_ref().is_some_and(|state| {
            state.reset_at.is_some_and(|reset_at| reset_at <= now)
                && window
                    .reset_at
                    .is_some_and(|reset_at| Some(reset_at) != state.reset_at && reset_at > now)
        });
        let cooldown_active = state.last_attempt_at.is_some_and(|last_attempt| {
            now - last_attempt < chrono::TimeDelta::minutes(i64::from(policy.cooldown_minutes))
        });
        if reset_completed && policy.reset_alerts && !cooldown_active {
            let notification = reset_notification_content(account, snapshot, window);
            self.storage
                .enqueue_notification(&notification.title, &notification.body)
                .await?;
            state.last_attempt_at = Some(now);
            debug!(
                provider_id = snapshot.provider_id.as_str(),
                account_id = account.id.as_str(),
                window_id = window.window_id,
                "limit reset notification queued"
            );
        }

        if window.reset_at.is_some() && state.reset_at != window.reset_at {
            state.notified_mask = 0;
            state.last_attempt_at = None;
        }
        let levels = notification_levels(&policy.thresholds);
        for (threshold, bit) in &levels {
            if percent > *threshold + REARM_HYSTERESIS {
                state.notified_mask &= !bit;
            }
        }

        let crossed_mask = crossed_mask(percent, &levels);
        let new_crossings = crossed_mask & !state.notified_mask;
        let cooldown_active = state.last_attempt_at.is_some_and(|last_attempt| {
            now - last_attempt < chrono::TimeDelta::minutes(i64::from(policy.cooldown_minutes))
        });
        if new_crossings != 0 && !cooldown_active {
            let threshold = most_severe_threshold(new_crossings, &levels);
            let notification = notification_content(account, snapshot, window, percent, threshold);
            self.storage
                .enqueue_notification(&notification.title, &notification.body)
                .await?;
            state.last_attempt_at = Some(now);
            state.notified_mask |= crossed_mask;
            debug!(
                provider_id = snapshot.provider_id.as_str(),
                account_id = account.id.as_str(),
                window_id = window.window_id,
                threshold,
                "usage notification queued"
            );
        }

        let cooldown_active = state.last_attempt_at.is_some_and(|last_attempt| {
            now - last_attempt < chrono::TimeDelta::minutes(i64::from(policy.cooldown_minutes))
        });
        let predictive_candidate = forecast.filter(|forecast| {
            policy.predictive_alerts
                && state.notified_mask & PREDICTIVE_NOTIFICATION_BIT == 0
                && forecast.status == ForecastStatus::AtRisk
                && forecast.confidence != ForecastConfidence::Low
                && forecast.predicted_exhaustion_at.is_some_and(|at| at > now)
        });
        if let Some(forecast) = predictive_candidate.filter(|_| !cooldown_active) {
            let notification = predictive_notification_content(account, snapshot, window, forecast);
            self.storage
                .enqueue_notification(&notification.title, &notification.body)
                .await?;
            state.last_attempt_at = Some(now);
            state.notified_mask |= PREDICTIVE_NOTIFICATION_BIT;
            debug!(
                provider_id = snapshot.provider_id.as_str(),
                account_id = account.id.as_str(),
                window_id = window.window_id,
                "predictive usage notification queued"
            );
        }

        state.reset_at = window.reset_at;
        if !notification_decision_state_changed(existing.as_ref(), &state) {
            return Ok(());
        }
        self.storage
            .upsert_notification_window_state(&account.id, &window.window_id, state)
            .await
    }
}

#[derive(Clone)]
struct ResolvedNotificationPolicy {
    enabled: bool,
    thresholds: Vec<u8>,
    reset_alerts: bool,
    predictive_alerts: bool,
    cooldown_minutes: u32,
    quiet_hours: Option<NotificationQuietHours>,
    snoozed_until: Option<DateTime<Utc>>,
}

impl ResolvedNotificationPolicy {
    fn for_window(config: &NotificationConfig, account: &Account, window: &UsageWindow) -> Self {
        let mut policy = Self {
            enabled: config.enabled,
            thresholds: config.thresholds_percent_remaining.clone(),
            reset_alerts: config.reset_alerts,
            predictive_alerts: config.predictive_alerts,
            cooldown_minutes: config.cooldown_minutes,
            quiet_hours: config.quiet_hours.clone(),
            snoozed_until: None,
        };
        for rule in &config.rules {
            let matches_account = rule
                .account_id
                .as_ref()
                .is_none_or(|account_id| account_id == &account.id);
            let matches_window = rule
                .window_id
                .as_ref()
                .is_none_or(|window_id| window_id == &window.window_id);
            if !matches_account || !matches_window {
                continue;
            }
            if let Some(enabled) = rule.enabled {
                policy.enabled = enabled;
            }
            if let Some(thresholds) = &rule.thresholds_percent_remaining {
                policy.thresholds = thresholds.clone();
            }
            if let Some(reset_alerts) = rule.reset_alerts {
                policy.reset_alerts = reset_alerts;
            }
            if let Some(predictive_alerts) = rule.predictive_alerts {
                policy.predictive_alerts = predictive_alerts;
            }
            if rule.snoozed_until.is_some() {
                policy.snoozed_until = rule.snoozed_until;
            }
        }
        policy
    }
}

fn is_quiet_now(hours: &NotificationQuietHours) -> bool {
    let hour = chrono::Local::now().hour() as u8;
    if hours.start_hour_local < hours.end_hour_local {
        hour >= hours.start_hour_local && hour < hours.end_hour_local
    } else {
        hour >= hours.start_hour_local || hour < hours.end_hour_local
    }
}

fn notification_decision_state_changed(
    existing: Option<&NotificationWindowState>,
    state: &NotificationWindowState,
) -> bool {
    match existing {
        Some(existing) => {
            existing.reset_at != state.reset_at
                || existing.notified_mask != state.notified_mask
                || existing.last_attempt_at != state.last_attempt_at
        }
        // The first observation establishes the reset cycle. Without it, an
        // account that has never crossed a low-usage threshold could not be
        // notified when that cycle completes.
        None => true,
    }
}

fn reset_notification_content(
    account: &Account,
    snapshot: &UsageSnapshot,
    window: &UsageWindow,
) -> DesktopNotification {
    let provider = provider_name(snapshot.provider_id.as_str());
    let window_name = window.label.trim().to_ascii_lowercase();
    let account_name = account
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&account.external_account_id);
    DesktopNotification {
        title: format!("{provider} {window_name} limit reset"),
        body: format!("{account_name} · Usage is available again"),
    }
}

fn notification_levels(thresholds: &[u8]) -> Vec<(f64, u8)> {
    let mut thresholds = thresholds.to_vec();
    thresholds.sort_unstable_by(|left, right| right.cmp(left));
    thresholds
        .into_iter()
        .enumerate()
        .map(|(index, threshold)| (f64::from(threshold), 1_u8 << index))
        .collect()
}

fn crossed_mask(percent: f64, levels: &[(f64, u8)]) -> u8 {
    levels
        .iter()
        .filter(|(threshold, _)| percent <= *threshold)
        .fold(0, |mask, (_, bit)| mask | bit)
}

fn most_severe_threshold(mask: u8, levels: &[(f64, u8)]) -> u8 {
    levels
        .iter()
        .rev()
        .find_map(|(threshold, bit)| (mask & bit != 0).then_some(*threshold as u8))
        .expect("a non-empty threshold mask has a level")
}

fn notification_content(
    account: &Account,
    snapshot: &UsageSnapshot,
    window: &UsageWindow,
    percent: f64,
    threshold: u8,
) -> DesktopNotification {
    let provider = provider_name(snapshot.provider_id.as_str());
    let window_name = window.label.trim().to_ascii_lowercase();
    let title = if threshold == 0 {
        format!("{provider} {window_name} usage exhausted")
    } else {
        format!("{provider} {window_name} usage is low")
    };
    let account_name = account
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&account.external_account_id);
    let remaining = if threshold == 0 {
        "no usage remaining".to_string()
    } else {
        format!("{:.0}% remaining", percent)
    };
    let mut parts = vec![account_name.to_string(), remaining];
    if let Some(reset_at) = window.reset_at {
        parts.push(format_reset(reset_at, Utc::now()));
    }
    DesktopNotification {
        title,
        body: parts.join(" · "),
    }
}

fn predictive_notification_content(
    account: &Account,
    snapshot: &UsageSnapshot,
    window: &UsageWindow,
    forecast: &UsageForecast,
) -> DesktopNotification {
    let provider = provider_name(snapshot.provider_id.as_str());
    let account_name = account
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&account.external_account_id);
    let exhaustion = forecast
        .predicted_exhaustion_at
        .map(|at| format_exhaustion(at, Utc::now()))
        .unwrap_or_else(|| "before reset".to_string());
    DesktopNotification {
        title: format!(
            "{provider} {} may run out",
            window.label.trim().to_ascii_lowercase()
        ),
        body: format!("{account_name} · Projected to exhaust {exhaustion}"),
    }
}

fn format_exhaustion(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = ((at - now).num_seconds().max(0) + 59) / 60;
    if minutes < 60 {
        format!("in {minutes}m")
    } else {
        format!("in {}h", minutes / 60)
    }
}

fn provider_name(provider_id: &str) -> String {
    match provider_id {
        "codex" => "Codex".to_string(),
        "claude" => "Claude".to_string(),
        "opencode_go" => "OpenCode Go".to_string(),
        value => value.to_string(),
    }
}

fn format_reset(reset_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (reset_at - now).num_seconds();
    if seconds <= 0 {
        return "reset pending".to_string();
    }
    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        return format!("resets in {minutes}m");
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours < 24 {
        return if remaining_minutes == 0 {
            format!("resets in {hours}h")
        } else {
            format!("resets in {hours}h {remaining_minutes}m")
        };
    }
    let days = hours / 24;
    let remaining_hours = hours % 24;
    if remaining_hours == 0 {
        format!("resets in {days}d")
    } else {
        format!("resets in {days}d {remaining_hours}h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use usage_core::{AccountDisplayNameSource, AccountId, ProviderId, UsageWindowKind};

    #[test]
    fn selects_only_the_most_severe_crossed_threshold() {
        let levels = notification_levels(&[50, 25, 10, 5, 0]);
        assert_eq!(crossed_mask(60.0, &levels), 0);
        assert_eq!(
            most_severe_threshold(crossed_mask(50.0, &levels), &levels),
            50
        );
        assert_eq!(
            most_severe_threshold(crossed_mask(4.0, &levels), &levels),
            5
        );
        assert_eq!(
            most_severe_threshold(crossed_mask(0.0, &levels), &levels),
            0
        );
    }

    #[tokio::test]
    async fn first_low_sample_alerts_once_and_restart_state_deduplicates() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let snapshot = test_snapshot(4.0, Utc::now() + TimeDelta::hours(2));

        manager.process_snapshot(&account, &snapshot).await;
        manager.process_snapshot(&account, &snapshot).await;
        let restarted = NotificationManager::new(storage.clone(), true);
        restarted.process_snapshot(&account, &snapshot).await;

        let notifications = storage.pending_notifications().await.unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].title.contains("usage is low"));
        assert!(notifications[0].body.contains("4% remaining"));
    }

    #[tokio::test]
    async fn first_high_sample_initializes_reset_state_without_alerting() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);

        manager
            .process_snapshot(
                &account,
                &test_snapshot(90.0, Utc::now() + TimeDelta::hours(2)),
            )
            .await;

        assert!(storage.pending_notifications().await.unwrap().is_empty());
        let state = storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.notified_mask, 0);
    }

    #[tokio::test]
    async fn unchanged_notification_decision_state_is_not_rewritten() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let reset_at = Utc::now() + TimeDelta::hours(2);

        manager
            .process_snapshot(&account, &test_snapshot(10.0, reset_at))
            .await;
        let first = storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .unwrap();

        manager
            .process_snapshot(&account, &test_snapshot(9.0, reset_at))
            .await;
        let second = storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn usage_recovery_persists_rearmed_thresholds() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let reset_at = Utc::now() + TimeDelta::hours(2);

        manager
            .process_snapshot(&account, &test_snapshot(10.0, reset_at))
            .await;
        manager
            .process_snapshot(&account, &test_snapshot(12.0, reset_at))
            .await;

        let state = storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.notified_mask & 4, 0);
    }

    #[tokio::test]
    async fn reset_rearms_thresholds() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let first_reset = Utc::now() + TimeDelta::hours(2);
        manager
            .process_snapshot(&account, &test_snapshot(24.0, first_reset))
            .await;
        manager
            .process_snapshot(
                &account,
                &test_snapshot(100.0, first_reset + TimeDelta::days(7)),
            )
            .await;
        manager
            .process_snapshot(
                &account,
                &test_snapshot(24.0, first_reset + TimeDelta::days(7)),
            )
            .await;
        assert_eq!(storage.pending_notifications().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn completed_reset_queues_one_notification_and_restart_deduplicates() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let previous_reset = Utc::now() - TimeDelta::minutes(1);
        let next_reset = Utc::now() + TimeDelta::days(7);

        manager
            .process_snapshot(&account, &test_snapshot(90.0, previous_reset))
            .await;
        manager
            .process_snapshot(&account, &test_snapshot(100.0, next_reset))
            .await;
        let restarted = NotificationManager::new(storage.clone(), true);
        restarted
            .process_snapshot(&account, &test_snapshot(100.0, next_reset))
            .await;

        let notifications = storage.pending_notifications().await.unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].title, "Codex weekly limit reset");
        assert_eq!(notifications[0].body, "Personal · Usage is available again");
    }

    #[tokio::test]
    async fn future_reset_time_correction_does_not_claim_limit_reset() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);

        manager
            .process_snapshot(
                &account,
                &test_snapshot(90.0, Utc::now() + TimeDelta::hours(2)),
            )
            .await;
        manager
            .process_snapshot(
                &account,
                &test_snapshot(90.0, Utc::now() + TimeDelta::hours(3)),
            )
            .await;

        assert!(storage.pending_notifications().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn acknowledged_notifications_are_removed_from_queue() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let snapshot = test_snapshot(10.0, Utc::now() + TimeDelta::hours(2));

        manager.process_snapshot(&account, &snapshot).await;
        let pending = storage.pending_notifications().await.unwrap();
        assert_eq!(pending.len(), 1);
        storage
            .acknowledge_notifications(&[pending[0].id])
            .await
            .unwrap();
        assert!(storage.pending_notifications().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabled_manager_does_not_send_or_initialize_state() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(storage.clone(), false);

        manager
            .process_snapshot(
                &account,
                &test_snapshot(0.0, Utc::now() + TimeDelta::hours(2)),
            )
            .await;

        assert!(storage.pending_notifications().await.unwrap().is_empty());
        assert!(storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn synthetic_local_quota_does_not_alert_or_initialize_state() {
        let storage = test_storage();
        let mut account = test_account();
        account.provider_id = ProviderId::new("opencode_go");
        let account = insert_account(&storage, &account).await;
        let manager = NotificationManager::new(storage.clone(), true);
        let mut snapshot = test_snapshot(0.0, Utc::now() + TimeDelta::hours(2));
        snapshot.provider_id = ProviderId::new("opencode_go");
        snapshot.account_id = account.id.clone();
        snapshot.metadata = serde_json::json!({
            "estimate": true,
            "web_authoritative": false,
        });

        manager.process_snapshot(&account, &snapshot).await;

        assert!(storage.pending_notifications().await.unwrap().is_empty());
        assert!(storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn predictive_alert_is_confidence_gated_and_once_per_reset_cycle() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let manager = NotificationManager::new(
            storage.clone(),
            NotificationConfig {
                predictive_alerts: true,
                cooldown_minutes: 0,
                ..NotificationConfig::default()
            },
        );
        let now = Utc::now();
        let snapshot = test_snapshot(80.0, now + TimeDelta::hours(2));
        let forecast = UsageForecast {
            provider_id: ProviderId::new("codex"),
            account_id: account.id.clone(),
            window_id: "weekly".to_string(),
            generated_at: now,
            reset_at: snapshot.windows[0].reset_at,
            current_percent_used: 20.0,
            expected_percent_used: Some(10.0),
            pace_delta_percent: Some(10.0),
            rate_percent_per_hour: Some(160.0),
            projected_percent_at_reset: Some(340.0),
            projected_percent_remaining_at_reset: Some(0.0),
            predicted_exhaustion_at: Some(now + TimeDelta::minutes(30)),
            status: ForecastStatus::AtRisk,
            sample_count: 6,
            confidence: ForecastConfidence::Medium,
        };

        manager
            .process_snapshot_with_forecasts(&account, &snapshot, std::slice::from_ref(&forecast))
            .await;
        manager
            .process_snapshot_with_forecasts(&account, &snapshot, &[forecast])
            .await;

        let notifications = storage.pending_notifications().await.unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].title.contains("may run out"));
        assert!(notifications[0].body.contains("Projected to exhaust"));
    }

    fn test_snapshot(percent: f64, reset_at: DateTime<Utc>) -> UsageSnapshot {
        UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("account-1"),
            collected_at: Utc::now(),
            windows: vec![UsageWindow {
                window_id: "weekly".to_string(),
                label: "Weekly".to_string(),
                kind: UsageWindowKind::Weekly,
                used: None,
                limit: None,
                remaining: None,
                percent_used: Some(100.0 - percent),
                percent_remaining: Some(percent),
                reset_at: Some(reset_at),
            }],
            metadata: serde_json::json!({}),
        }
    }

    fn test_account() -> Account {
        let now = Utc::now();
        Account {
            id: AccountId::new("account-1"),
            provider_id: ProviderId::new("codex"),
            external_account_id: "external-1".to_string(),
            profile_id: Some("default".to_string()),
            display_name: Some("Personal".to_string()),
            display_name_source: AccountDisplayNameSource::User,
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    async fn insert_account(storage: &Storage, account: &Account) -> Account {
        storage
            .upsert_account(
                &account.provider_id,
                &account.external_account_id,
                account.profile_id.as_deref(),
                account.display_name.as_deref(),
                None,
            )
            .await
            .unwrap()
    }

    fn test_storage() -> Storage {
        let path =
            std::env::temp_dir().join(format!("usage-notify-{}.sqlite3", uuid::Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
