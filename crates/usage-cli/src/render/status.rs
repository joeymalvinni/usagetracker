use std::collections::HashMap;
#[cfg(test)]
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;
use std::hash::Hash;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
#[cfg(test)]
use usage_core::ProviderHealthStatus;
use usage_core::{Account, ConfigResponse, ProviderHealth, UsageSnapshot};

use crate::{
    render::{
        labels::identity_labels,
        style::{
            format_collection_mode, format_local_time, format_provider_name, json_name, push_kv,
            relative_time_opt, Theme,
        },
        table::Table,
    },
    OutputStyle,
};

#[derive(Debug, Serialize)]
pub struct StatusView {
    #[serde(rename = "type")]
    response_type: &'static str,
    daemon: &'static str,
    socket_path: String,
    poll_interval_seconds: u64,
    enabled_provider_count: usize,
    updated_at: Option<DateTime<Utc>>,
    providers: Vec<ProviderStatusRow>,
}

#[derive(Debug, Serialize)]
struct ProviderStatusRow {
    provider_id: String,
    account_id: Option<String>,
    identity: Option<String>,
    plan: Option<String>,
    state: String,
    usage: UsageFreshness,
    last_success_at: Option<DateTime<Utc>>,
    last_update_at: Option<DateTime<Utc>>,
    detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UsageFreshness {
    Fresh,
    Stale,
    Missing,
}

struct StatusIndexes<'a> {
    accounts_by_provider: HashMap<&'a str, Vec<&'a Account>>,
    latest_by_account: HashMap<(&'a str, &'a str), &'a UsageSnapshot>,
    latest_by_provider: HashMap<&'a str, &'a UsageSnapshot>,
    health_by_account: HashMap<(&'a str, Option<&'a str>), &'a ProviderHealth>,
}

impl<'a> StatusIndexes<'a> {
    fn new(
        snapshots: &'a [UsageSnapshot],
        accounts: &'a [Account],
        health: &'a [ProviderHealth],
    ) -> Self {
        let mut accounts_by_provider: HashMap<&str, Vec<&Account>> = HashMap::new();
        for account in accounts.iter().filter(|account| !account.hidden) {
            accounts_by_provider
                .entry(account.provider_id.as_str())
                .or_default()
                .push(account);
        }

        let mut latest_by_account = HashMap::with_capacity(snapshots.len());
        let mut latest_by_provider = HashMap::new();
        for snapshot in snapshots {
            insert_latest_snapshot(
                &mut latest_by_account,
                (snapshot.provider_id.as_str(), snapshot.account_id.as_str()),
                snapshot,
            );
            insert_latest_snapshot(
                &mut latest_by_provider,
                snapshot.provider_id.as_str(),
                snapshot,
            );
        }

        let mut health_by_account = HashMap::with_capacity(health.len());
        for row in health {
            health_by_account
                .entry((
                    row.provider_id.as_str(),
                    row.account_id.as_ref().map(|id| id.as_str()),
                ))
                .or_insert(row);
        }

        Self {
            accounts_by_provider,
            latest_by_account,
            latest_by_provider,
            health_by_account,
        }
    }
}

fn insert_latest_snapshot<'a, K: Eq + Hash>(
    snapshots: &mut HashMap<K, &'a UsageSnapshot>,
    key: K,
    candidate: &'a UsageSnapshot,
) {
    match snapshots.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            // Iterator::max_by_key selects the last equal maximum. Keep that
            // exact tie behavior while building the index in input order.
            if candidate.collected_at >= entry.get().collected_at {
                entry.insert(candidate);
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(candidate);
        }
    }
}

impl StatusView {
    #[cfg(test)]
    pub fn from_parts(
        socket_path: String,
        snapshots: &[UsageSnapshot],
        accounts: &[Account],
        health: &[ProviderHealth],
        config: &ConfigResponse,
    ) -> Self {
        let provider_ids = provider_ids(config, health, snapshots, accounts);
        Self::from_selected_parts(
            socket_path,
            snapshots,
            accounts,
            health,
            config,
            &provider_ids,
        )
    }

    pub fn from_selected_parts(
        socket_path: String,
        snapshots: &[UsageSnapshot],
        accounts: &[Account],
        health: &[ProviderHealth],
        config: &ConfigResponse,
        provider_ids: &[String],
    ) -> Self {
        let indexes = StatusIndexes::new(snapshots, accounts, health);
        let freshness_window = TimeDelta::seconds((config.poll_interval_seconds * 2) as i64);

        let mut providers = Vec::with_capacity(accounts.len().max(provider_ids.len()));
        for provider_id in provider_ids {
            let provider_enabled = config
                .providers
                .get(provider_id)
                .is_some_and(|provider| provider.enabled);
            let provider_accounts = indexes
                .accounts_by_provider
                .get(provider_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            if provider_accounts.is_empty() {
                providers.push(provider_status_row(
                    provider_id,
                    None,
                    &indexes,
                    freshness_window,
                    provider_enabled,
                ));
            } else {
                providers.extend(provider_accounts.iter().map(|account| {
                    provider_status_row(
                        provider_id,
                        Some(*account),
                        &indexes,
                        freshness_window,
                        provider_enabled,
                    )
                }));
            }
        }

        let updated_at = providers
            .iter()
            .filter_map(|row| row.last_success_at.max(row.last_update_at))
            .max()
            .or_else(|| health.iter().map(|row| row.updated_at).max());

        Self {
            response_type: "status",
            daemon: "connected",
            socket_path,
            poll_interval_seconds: config.poll_interval_seconds,
            enabled_provider_count: config
                .providers
                .values()
                .filter(|provider| provider.enabled)
                .count(),
            updated_at,
            providers,
        }
    }
}

fn provider_status_row(
    provider_id: &str,
    account: Option<&Account>,
    indexes: &StatusIndexes<'_>,
    freshness_window: TimeDelta,
    provider_enabled: bool,
) -> ProviderStatusRow {
    let account_id = account.map(|account| account.id.as_str());
    let latest = match account_id {
        Some(account_id) => indexes
            .latest_by_account
            .get(&(provider_id, account_id))
            .copied(),
        None => indexes.latest_by_provider.get(provider_id).copied(),
    };
    let provider_health = indexes
        .health_by_account
        .get(&(provider_id, account_id))
        .or_else(|| indexes.health_by_account.get(&(provider_id, None)))
        .copied();
    let labels = identity_labels(account, latest);
    let last_update_at = latest.map(|snapshot| snapshot.collected_at);
    ProviderStatusRow {
        provider_id: provider_id.to_string(),
        account_id: account_id.map(str::to_string),
        identity: labels.identity,
        plan: labels.plan,
        state: (!provider_enabled)
            .then(|| "disabled".to_string())
            .or_else(|| {
                account
                    .filter(|account| !account.collection_enabled)
                    .map(|_| "disabled".to_string())
            })
            .or_else(|| provider_health.map(|row| json_name(&row.status)))
            .unwrap_or_else(|| "ok".to_string()),
        usage: usage_freshness(last_update_at, freshness_window),
        last_success_at: provider_health.and_then(|row| row.last_success_at),
        last_update_at,
        detail: status_detail(provider_id, provider_health),
    }
}

#[cfg(test)]
pub fn render_status(status: &StatusView, style: OutputStyle, color: bool) -> String {
    render_status_with_width(status, style, color, usize::MAX)
}

pub fn render_status_with_width(
    status: &StatusView,
    style: OutputStyle,
    color: bool,
    width: usize,
) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => render_status_dashboard(status, theme, width),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

fn render_status_dashboard(status: &StatusView, theme: Theme, width: usize) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "{}", theme.title("Usage Tracker"));
    output.push('\n');
    push_kv(&mut output, theme, "Daemon", status.daemon);
    push_kv(&mut output, theme, "Socket", &status.socket_path);
    push_kv(
        &mut output,
        theme,
        "Poll",
        &format!("every {}s", status.poll_interval_seconds),
    );
    push_kv(
        &mut output,
        theme,
        "Providers",
        &format!("{} enabled", status.enabled_provider_count),
    );
    push_kv(
        &mut output,
        theme,
        "Updated",
        &format_local_time(status.updated_at),
    );

    output.push('\n');
    let mut table = Table::new([
        "Provider", "Identity", "Plan", "State", "Usage", "Updated", "Detail",
    ]);
    for row in &status.providers {
        table.row([
            format_provider_name(&row.provider_id),
            row.identity.clone().unwrap_or_else(|| "-".to_string()),
            row.plan.clone().unwrap_or_else(|| "-".to_string()),
            theme.status(&row.state),
            freshness_text(row.usage, theme),
            relative_time_opt(row.last_success_at.max(row.last_update_at)),
            row.detail.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    output.push_str(&table.render_with_width(theme, width));
    output.trim_end().to_string()
}

fn freshness_text(freshness: UsageFreshness, theme: Theme) -> String {
    match freshness {
        UsageFreshness::Fresh => theme.good("fresh"),
        UsageFreshness::Stale => theme.warn("stale"),
        UsageFreshness::Missing => theme.warn("missing"),
    }
}

fn usage_freshness(
    last_update_at: Option<DateTime<Utc>>,
    freshness_window: TimeDelta,
) -> UsageFreshness {
    let Some(last_update_at) = last_update_at else {
        return UsageFreshness::Missing;
    };
    if Utc::now() - last_update_at <= freshness_window {
        UsageFreshness::Fresh
    } else {
        UsageFreshness::Stale
    }
}

fn status_detail(provider_id: &str, health: Option<&ProviderHealth>) -> Option<String> {
    let health = health?;
    health.last_error_message.clone().or_else(|| {
        health
            .collection_mode
            .as_deref()
            .map(|mode| format_collection_mode(provider_id, mode))
    })
}

#[cfg(test)]
fn provider_ids(
    config: &ConfigResponse,
    health: &[ProviderHealth],
    snapshots: &[UsageSnapshot],
    accounts: &[Account],
) -> Vec<String> {
    let providers_with_data = snapshots
        .iter()
        .map(|snapshot| snapshot.provider_id.as_str())
        .chain(accounts.iter().map(|account| account.provider_id.as_str()))
        .collect::<HashSet<_>>();
    let unavailable_without_data = health
        .iter()
        .filter(|row| {
            matches!(row.status, ProviderHealthStatus::ProviderError)
                && row.last_error_code.as_deref() == Some("provider_unavailable")
        })
        .map(|row| row.provider_id.as_str())
        .collect::<HashSet<_>>();
    let mut provider_ids = BTreeSet::new();
    provider_ids.extend(config.providers.keys().cloned());
    provider_ids.extend(
        health
            .iter()
            .map(|row| row.provider_id.as_str().to_string()),
    );
    provider_ids.extend(
        snapshots
            .iter()
            .map(|snapshot| snapshot.provider_id.as_str().to_string()),
    );
    provider_ids.extend(
        accounts
            .iter()
            .map(|account| account.provider_id.as_str().to_string()),
    );
    provider_ids
        .into_iter()
        .filter(|provider_id| {
            is_enabled_provider(provider_id, config)
                && (providers_with_data.contains(provider_id.as_str())
                    || !unavailable_without_data.contains(provider_id.as_str()))
        })
        .collect()
}

#[cfg(test)]
fn is_enabled_provider(provider_id: &str, config: &ConfigResponse) -> bool {
    config
        .providers
        .get(provider_id)
        .is_some_and(|provider| provider.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use usage_core::{AccountId, ProviderHealthStatus, ProviderId, ProviderToggle, UsageWindow};

    #[test]
    fn renders_status_dashboard_with_fresh_provider() {
        let status = sample_status(Utc::now());
        let rendered = render_status(&status, OutputStyle::Dashboard, false);

        assert!(rendered.contains("Usage Tracker"));
        assert!(rendered.contains("Daemon"));
        assert!(rendered.contains("fresh"));
        assert!(rendered.contains("terminal"));
        assert!(rendered.contains("joey"));
        assert!(rendered.contains("Team"));
        assert!(!rendered.contains("Claude team"));
    }

    #[test]
    fn serializes_status_json_shape() {
        let status = sample_status(Utc::now());
        let rendered = serde_json::to_string(&status).unwrap();

        assert!(rendered.contains(r#""type":"status""#));
        assert!(rendered.contains(r#""usage":"fresh""#));
    }

    #[test]
    fn hides_disabled_providers_without_data() {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("codex".to_string(), ProviderToggle { enabled: false });
        providers.insert("claude".to_string(), ProviderToggle { enabled: true });
        let health = ProviderHealth {
            provider_id: ProviderId::new("codex"),
            account_id: None,
            status: ProviderHealthStatus::Disabled,
            collection_mode: None,
            last_success_at: None,
            last_failure_at: None,
            last_error_code: None,
            last_error_message: None,
            updated_at: Utc::now(),
        };

        let status = StatusView::from_parts(
            "/tmp/usage.sock".to_string(),
            &[],
            &[],
            &[health],
            &ConfigResponse {
                poll_interval_seconds: 60,
                notifications: Default::default(),
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                providers,
            },
        );
        let rendered = serde_json::to_string(&status).unwrap();

        assert!(!rendered.contains(r#""provider_id":"codex""#));
        assert!(rendered.contains(r#""provider_id":"claude""#));
    }

    #[test]
    fn hides_disabled_providers_with_data() {
        let account_id = AccountId::new("account");
        let account = Account {
            id: account_id.clone(),
            provider_id: ProviderId::new("codex"),
            external_account_id: "joey".to_string(),
            profile_id: None,
            display_name: None,
            display_name_source: Default::default(),
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id,
            collected_at: Utc::now(),
            windows: Vec::<UsageWindow>::new(),
            metadata: json!({}),
        };
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("codex".to_string(), ProviderToggle { enabled: false });

        let status = StatusView::from_parts(
            "/tmp/usage.sock".to_string(),
            &[snapshot],
            &[account],
            &[],
            &ConfigResponse {
                poll_interval_seconds: 60,
                notifications: Default::default(),
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                providers,
            },
        );
        let rendered = serde_json::to_string(&status).unwrap();

        assert!(!rendered.contains(r#""provider_id":"codex""#));
    }

    #[test]
    fn hides_unavailable_enabled_providers_without_data() {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("codex".to_string(), ProviderToggle { enabled: true });
        let health = ProviderHealth {
            provider_id: ProviderId::new("codex"),
            account_id: None,
            status: ProviderHealthStatus::ProviderError,
            collection_mode: None,
            last_success_at: None,
            last_failure_at: Some(Utc::now()),
            last_error_code: Some("provider_unavailable".to_string()),
            last_error_message: Some("Codex data directory not found".to_string()),
            updated_at: Utc::now(),
        };

        let status = StatusView::from_parts(
            "/tmp/usage.sock".to_string(),
            &[],
            &[],
            &[health],
            &ConfigResponse {
                poll_interval_seconds: 60,
                notifications: Default::default(),
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                providers,
            },
        );
        let rendered = serde_json::to_string(&status).unwrap();

        assert!(!rendered.contains(r#""provider_id":"codex""#));
    }

    #[test]
    fn selected_status_includes_no_data_and_disabled_providers() {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("codex".to_string(), ProviderToggle { enabled: true });
        providers.insert("grok".to_string(), ProviderToggle { enabled: false });
        let config = ConfigResponse {
            poll_interval_seconds: 60,
            notifications: Default::default(),
            config_path: "/tmp/config.json".to_string(),
            socket_path: "/tmp/usage.sock".to_string(),
            db_path: "/tmp/usage.sqlite3".to_string(),
            providers,
        };

        let status = StatusView::from_selected_parts(
            "/tmp/usage.sock".to_string(),
            &[],
            &[],
            &[],
            &config,
            &["codex".to_string(), "grok".to_string()],
        );
        let rendered = serde_json::to_value(&status).unwrap();

        assert_eq!(rendered["providers"][0]["state"], "ok");
        assert_eq!(rendered["providers"][1]["state"], "disabled");
    }

    #[test]
    fn indexed_status_preserves_snapshot_ties_health_precedence_and_account_order() {
        let collected_at = Utc::now();
        let account_a = Account {
            id: AccountId::new("a"),
            provider_id: ProviderId::new("codex"),
            external_account_id: "external-a".to_string(),
            profile_id: None,
            display_name: None,
            display_name_source: Default::default(),
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: collected_at,
            updated_at: collected_at,
        };
        let account_b = Account {
            id: AccountId::new("b"),
            provider_id: ProviderId::new("codex"),
            external_account_id: "external-b".to_string(),
            profile_id: None,
            display_name: None,
            display_name_source: Default::default(),
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: collected_at,
            updated_at: collected_at,
        };
        let snapshots = vec![
            UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("a"),
                collected_at,
                windows: Vec::new(),
                metadata: json!({"email": "first@example.com"}),
            },
            UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("a"),
                collected_at,
                windows: Vec::new(),
                metadata: json!({"email": "second@example.com"}),
            },
            UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("b"),
                collected_at,
                windows: Vec::new(),
                metadata: json!({"email": "b@example.com"}),
            },
        ];
        let health = vec![
            ProviderHealth {
                provider_id: ProviderId::new("codex"),
                account_id: None,
                status: ProviderHealthStatus::RateLimited,
                collection_mode: None,
                last_success_at: None,
                last_failure_at: Some(collected_at),
                last_error_code: Some("fallback".to_string()),
                last_error_message: Some("provider fallback".to_string()),
                updated_at: collected_at,
            },
            ProviderHealth {
                provider_id: ProviderId::new("codex"),
                account_id: Some(AccountId::new("a")),
                status: ProviderHealthStatus::Ok,
                collection_mode: None,
                last_success_at: Some(collected_at),
                last_failure_at: None,
                last_error_code: None,
                last_error_message: Some("first exact".to_string()),
                updated_at: collected_at,
            },
            ProviderHealth {
                provider_id: ProviderId::new("codex"),
                account_id: Some(AccountId::new("a")),
                status: ProviderHealthStatus::AuthFailed,
                collection_mode: None,
                last_success_at: None,
                last_failure_at: Some(collected_at),
                last_error_code: Some("second".to_string()),
                last_error_message: Some("second exact".to_string()),
                updated_at: collected_at,
            },
        ];
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("codex".to_string(), ProviderToggle { enabled: true });
        let config = ConfigResponse {
            poll_interval_seconds: 60,
            notifications: Default::default(),
            config_path: "/tmp/config.json".to_string(),
            socket_path: "/tmp/usage.sock".to_string(),
            db_path: "/tmp/usage.sqlite3".to_string(),
            providers,
        };

        let status = StatusView::from_parts(
            "/tmp/usage.sock".to_string(),
            &snapshots,
            &[account_a, account_b],
            &health,
            &config,
        );

        assert_eq!(status.providers.len(), 2);
        assert_eq!(status.providers[0].account_id.as_deref(), Some("a"));
        assert_eq!(status.providers[1].account_id.as_deref(), Some("b"));
        assert_eq!(
            status.providers[0].identity.as_deref(),
            Some("second@example.com")
        );
        assert_eq!(status.providers[0].state, "ok");
        assert_eq!(status.providers[0].detail.as_deref(), Some("first exact"));
        assert_eq!(status.providers[1].state, "rate_limited");
        assert_eq!(
            status.providers[1].detail.as_deref(),
            Some("provider fallback")
        );
    }

    fn sample_status(collected_at: DateTime<Utc>) -> StatusView {
        let account_id = AccountId::new("account");
        let account = Account {
            id: account_id.clone(),
            provider_id: ProviderId::new("claude"),
            external_account_id: "joey".to_string(),
            profile_id: None,
            display_name: Some("Claude team".to_string()),
            display_name_source: Default::default(),
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("claude"),
            account_id: account_id.clone(),
            collected_at,
            windows: Vec::<UsageWindow>::new(),
            metadata: json!({
                "collection_mode": "claude_cli_usage",
                "credential_profile": "joey",
                "subscription_type": "team",
            }),
        };
        let health = ProviderHealth {
            provider_id: ProviderId::new("claude"),
            account_id: Some(account_id),
            status: ProviderHealthStatus::Ok,
            collection_mode: Some("claude_cli_usage".to_string()),
            last_success_at: Some(Utc::now()),
            last_failure_at: None,
            last_error_code: None,
            last_error_message: None,
            updated_at: Utc::now(),
        };
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("claude".to_string(), ProviderToggle { enabled: true });
        let config = ConfigResponse {
            poll_interval_seconds: 300,
            notifications: Default::default(),
            config_path: "/tmp/config.json".to_string(),
            socket_path: "/tmp/usage.sock".to_string(),
            db_path: "/tmp/usage.sqlite3".to_string(),
            providers,
        };

        StatusView::from_parts(
            "/tmp/usage.sock".to_string(),
            &[snapshot],
            &[account],
            &[health],
            &config,
        )
    }
}
