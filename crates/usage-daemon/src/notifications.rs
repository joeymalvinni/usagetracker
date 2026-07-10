use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use tracing::{debug, warn};
use usage_core::{Account, UsageSnapshot, UsageWindow};

use crate::storage::{NotificationWindowState, Storage};

const RETRY_COOLDOWN: Duration = Duration::from_secs(15 * 60);
const REARM_HYSTERESIS: f64 = 1.0;
const LEVELS: [(f64, u8); 5] = [(50.0, 1), (25.0, 2), (10.0, 4), (5.0, 8), (0.0, 16)];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopNotification {
    pub title: String,
    pub body: String,
}

pub trait NotificationSender: Send + Sync {
    fn send(&self, notification: &DesktopNotification) -> anyhow::Result<()>;
}

pub struct NativeNotificationSender;

impl NotificationSender for NativeNotificationSender {
    fn send(&self, notification: &DesktopNotification) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        if !notify_rust::request_auth_blocking()? {
            anyhow::bail!("notification permission was not granted");
        }
        notify_rust::Notification::new()
            .appname("Usage Tracker")
            .summary(&notification.title)
            .body(&notification.body)
            .show()?;
        Ok(())
    }
}

pub struct NotificationManager {
    storage: Storage,
    enabled: AtomicBool,
    sender: Arc<dyn NotificationSender>,
}

impl NotificationManager {
    pub fn new(storage: Storage, enabled: bool) -> Arc<Self> {
        Self::with_sender(storage, enabled, Arc::new(NativeNotificationSender))
    }

    fn with_sender(
        storage: Storage,
        enabled: bool,
        sender: Arc<dyn NotificationSender>,
    ) -> Arc<Self> {
        Arc::new(Self {
            storage,
            enabled: AtomicBool::new(enabled),
            sender,
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub async fn process_snapshot(&self, account: &Account, snapshot: &UsageSnapshot) {
        if !self.enabled() {
            return;
        }
        for window in &snapshot.windows {
            if let Err(err) = self.process_window(account, snapshot, window).await {
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
    ) -> anyhow::Result<()> {
        let Some(percent) = window.percent_remaining.filter(|value| value.is_finite()) else {
            return Ok(());
        };
        let percent = percent.clamp(0.0, 100.0);
        let now = Utc::now();
        let existing = self
            .storage
            .notification_window_state(&account.id, &window.window_id)
            .await?;
        let mut state = existing.unwrap_or(NotificationWindowState {
            last_percent: percent,
            reset_at: window.reset_at,
            notified_mask: 0,
            last_attempt_at: None,
        });

        if window.reset_at.is_some() && state.reset_at != window.reset_at {
            state.notified_mask = 0;
            state.last_attempt_at = None;
        }
        for (threshold, bit) in LEVELS {
            if percent > threshold + REARM_HYSTERESIS {
                state.notified_mask &= !bit;
            }
        }

        let crossed_mask = crossed_mask(percent);
        let new_crossings = crossed_mask & !state.notified_mask;
        let cooling_down = state.last_attempt_at.is_some_and(|attempt| {
            now.signed_duration_since(attempt)
                .to_std()
                .is_ok_and(|elapsed| elapsed < RETRY_COOLDOWN)
        });

        if new_crossings != 0 && !cooling_down {
            let threshold = most_severe_threshold(new_crossings);
            let notification = notification_content(account, snapshot, window, percent, threshold);
            let sender = self.sender.clone();
            let send_result =
                tokio::task::spawn_blocking(move || sender.send(&notification)).await?;
            state.last_attempt_at = Some(now);
            match send_result {
                Ok(()) => {
                    state.notified_mask |= crossed_mask;
                    debug!(
                        provider_id = snapshot.provider_id.as_str(),
                        account_id = account.id.as_str(),
                        window_id = window.window_id,
                        threshold,
                        "usage notification delivered"
                    );
                }
                Err(err) => warn!(
                    provider_id = snapshot.provider_id.as_str(),
                    account_id = account.id.as_str(),
                    window_id = window.window_id,
                    error = %err,
                    "desktop notification delivery failed"
                ),
            }
        }

        state.last_percent = percent;
        state.reset_at = window.reset_at;
        self.storage
            .upsert_notification_window_state(&account.id, &window.window_id, state)
            .await
    }
}

fn crossed_mask(percent: f64) -> u8 {
    LEVELS
        .iter()
        .filter(|(threshold, _)| percent <= *threshold)
        .fold(0, |mask, (_, bit)| mask | bit)
}

fn most_severe_threshold(mask: u8) -> u8 {
    LEVELS
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
    use std::sync::Mutex;
    use usage_core::{AccountDisplayNameSource, AccountId, ProviderId, UsageWindowKind};

    #[derive(Default)]
    struct RecordingSender(Mutex<Vec<DesktopNotification>>);

    impl NotificationSender for RecordingSender {
        fn send(&self, notification: &DesktopNotification) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(notification.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailingSender(Mutex<usize>);

    impl NotificationSender for FailingSender {
        fn send(&self, _notification: &DesktopNotification) -> anyhow::Result<()> {
            *self.0.lock().unwrap() += 1;
            anyhow::bail!("notifications unavailable")
        }
    }

    #[test]
    fn selects_only_the_most_severe_crossed_threshold() {
        assert_eq!(crossed_mask(60.0), 0);
        assert_eq!(most_severe_threshold(crossed_mask(50.0)), 50);
        assert_eq!(most_severe_threshold(crossed_mask(4.0)), 5);
        assert_eq!(most_severe_threshold(crossed_mask(0.0)), 0);
    }

    #[tokio::test]
    async fn first_low_sample_alerts_once_and_restart_state_deduplicates() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let sender = Arc::new(RecordingSender::default());
        let manager = NotificationManager::with_sender(storage.clone(), true, sender.clone());
        let snapshot = test_snapshot(4.0, Utc::now() + TimeDelta::hours(2));

        manager.process_snapshot(&account, &snapshot).await;
        manager.process_snapshot(&account, &snapshot).await;
        let restarted = NotificationManager::with_sender(storage, true, sender.clone());
        restarted.process_snapshot(&account, &snapshot).await;

        let notifications = sender.0.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].title.contains("usage is low"));
        assert!(notifications[0].body.contains("4% remaining"));
    }

    #[tokio::test]
    async fn reset_rearms_thresholds() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let sender = Arc::new(RecordingSender::default());
        let manager = NotificationManager::with_sender(storage, true, sender.clone());
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
        assert_eq!(sender.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn delivery_failure_uses_retry_cooldown_without_marking_crossed() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let sender = Arc::new(FailingSender::default());
        let manager = NotificationManager::with_sender(storage.clone(), true, sender.clone());
        let snapshot = test_snapshot(10.0, Utc::now() + TimeDelta::hours(2));

        manager.process_snapshot(&account, &snapshot).await;
        manager.process_snapshot(&account, &snapshot).await;

        assert_eq!(*sender.0.lock().unwrap(), 1);
        let state = storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.notified_mask, 0);
        assert!(state.last_attempt_at.is_some());
    }

    #[tokio::test]
    async fn disabled_manager_does_not_send_or_initialize_state() {
        let storage = test_storage();
        let account = insert_account(&storage, &test_account()).await;
        let sender = Arc::new(RecordingSender::default());
        let manager = NotificationManager::with_sender(storage.clone(), false, sender.clone());

        manager
            .process_snapshot(
                &account,
                &test_snapshot(0.0, Utc::now() + TimeDelta::hours(2)),
            )
            .await;

        assert!(sender.0.lock().unwrap().is_empty());
        assert!(storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .is_none());
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
