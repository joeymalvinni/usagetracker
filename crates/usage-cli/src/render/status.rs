use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use usage_core::{Account, ConfigResponse, ProviderHealth, ProviderHealthStatus, UsageSnapshot};

use crate::{
    render::{
        labels::identity_labels,
        style::{format_collection_mode, format_local_time, format_provider_name, Theme},
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

impl StatusView {
    pub fn from_parts(
        socket_path: String,
        snapshots: &[UsageSnapshot],
        accounts: &[Account],
        health: &[ProviderHealth],
        config: &ConfigResponse,
    ) -> Self {
        let account_by_id = account_by_id(accounts);
        let latest_snapshots = latest_snapshots_by_provider(snapshots);
        let health_by_provider = health
            .iter()
            .map(|row| (row.provider_id.as_str().to_string(), row))
            .collect::<HashMap<_, _>>();
        let provider_ids = provider_ids(config, health, snapshots, accounts);
        let freshness_window = TimeDelta::seconds((config.poll_interval_seconds * 2) as i64);

        let providers = provider_ids
            .into_iter()
            .map(|provider_id| {
                let latest = latest_snapshots.get(&provider_id).copied();
                let health = health_by_provider.get(&provider_id).copied();
                let account_model = health
                    .and_then(|row| row.account_id.as_ref())
                    .or_else(|| latest.map(|snapshot| &snapshot.account_id))
                    .and_then(|id| account_by_id.get(id.as_str()).copied());
                let labels = identity_labels(account_model, latest);
                let last_update_at = latest.map(|snapshot| snapshot.collected_at);
                ProviderStatusRow {
                    provider_id: provider_id.clone(),
                    identity: labels.identity,
                    plan: labels.plan,
                    state: health
                        .map(|row| json_name(&row.status))
                        .unwrap_or_else(|| "unknown".to_string()),
                    usage: usage_freshness(last_update_at, freshness_window),
                    last_success_at: health.and_then(|row| row.last_success_at),
                    last_update_at,
                    detail: status_detail(provider_id.as_str(), health),
                }
            })
            .collect::<Vec<_>>();

        let updated_at = providers
            .iter()
            .filter_map(|row| row.last_success_at.max(row.last_update_at))
            .max()
            .or_else(|| health.iter().map(|row| row.updated_at).max());

        Self {
            response_type: "status",
            daemon: "ok",
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

pub fn render_status(status: &StatusView, style: OutputStyle, color: bool) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => render_status_dashboard(status, theme),
        OutputStyle::Compact => render_status_compact(status, theme),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

fn render_status_dashboard(status: &StatusView, theme: Theme) -> String {
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
        "Provider",
        "Identity",
        "Plan",
        "State",
        "Usage",
        "Last success",
        "Last update",
        "Detail",
    ]);
    for row in &status.providers {
        table.row([
            format_provider_name(&row.provider_id),
            row.identity.clone().unwrap_or_else(|| "-".to_string()),
            row.plan.clone().unwrap_or_else(|| "-".to_string()),
            theme.status(&row.state),
            freshness_text(row.usage, theme),
            format_local_time(row.last_success_at),
            format_local_time(row.last_update_at),
            row.detail.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    output.push_str(&table.render(theme));
    output.trim_end().to_string()
}

fn render_status_compact(status: &StatusView, theme: Theme) -> String {
    let mut lines = vec![format!(
        "{} {} · poll {}s · providers {} enabled · updated {}",
        theme.title("Daemon"),
        theme.good(status.daemon),
        status.poll_interval_seconds,
        status.enabled_provider_count,
        format_local_time(status.updated_at)
    )];
    lines.extend(status.providers.iter().map(|row| {
        let identity = row
            .identity
            .as_ref()
            .map(|identity| format!(" · {identity}"))
            .unwrap_or_default();
        let plan = row
            .plan
            .as_ref()
            .map(|plan| format!(" · {plan}"))
            .unwrap_or_default();
        let detail = row
            .detail
            .as_ref()
            .map(|detail| format!(" · {detail}"))
            .unwrap_or_default();
        format!(
            "{}{}{}: {} · {}{} · success {}",
            theme.title(&format_provider_name(&row.provider_id)),
            identity,
            plan,
            theme.status(&row.state),
            freshness_text(row.usage, theme),
            detail,
            format_local_time(row.last_success_at)
        )
    }));
    lines.join("\n")
}

fn push_kv(output: &mut String, theme: Theme, key: &str, value: &str) {
    let _ = writeln!(output, "{}  {}", theme.label(&format!("{key:<9}")), value);
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

fn provider_ids(
    config: &ConfigResponse,
    health: &[ProviderHealth],
    snapshots: &[UsageSnapshot],
    accounts: &[Account],
) -> Vec<String> {
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
                && (has_provider_data(provider_id, snapshots, accounts)
                    || !is_unavailable_without_data(provider_id, health))
        })
        .collect()
}

fn has_provider_data(provider_id: &str, snapshots: &[UsageSnapshot], accounts: &[Account]) -> bool {
    snapshots
        .iter()
        .any(|snapshot| snapshot.provider_id.as_str() == provider_id)
        || accounts
            .iter()
            .any(|account| account.provider_id.as_str() == provider_id)
}

fn is_enabled_provider(provider_id: &str, config: &ConfigResponse) -> bool {
    config
        .providers
        .get(provider_id)
        .is_some_and(|provider| provider.enabled)
        || config
            .enabled_providers
            .iter()
            .any(|id| id.as_str() == provider_id)
}

fn is_unavailable_without_data(provider_id: &str, health: &[ProviderHealth]) -> bool {
    health.iter().any(|row| {
        row.provider_id.as_str() == provider_id
            && matches!(row.status, ProviderHealthStatus::ProviderError)
            && row.last_error_code.as_deref() == Some("provider_unavailable")
    })
}

fn latest_snapshots_by_provider(snapshots: &[UsageSnapshot]) -> HashMap<String, &UsageSnapshot> {
    let mut latest = HashMap::new();
    for snapshot in snapshots {
        latest
            .entry(snapshot.provider_id.as_str().to_string())
            .and_modify(|current: &mut &UsageSnapshot| {
                if snapshot.collected_at > current.collected_at {
                    *current = snapshot;
                }
            })
            .or_insert(snapshot);
    }
    latest
}

fn account_by_id(accounts: &[Account]) -> HashMap<String, &Account> {
    accounts
        .iter()
        .map(|account| (account.id.as_str().to_string(), account))
        .collect()
}

fn json_name(value: &impl Serialize) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"unknown\"".to_string())
        .trim_matches('"')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
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
    fn renders_status_compact_with_stale_provider() {
        let status = sample_status(Utc::now() - TimeDelta::seconds(700));
        let rendered = render_status(&status, OutputStyle::Compact, false);

        assert!(rendered.contains("Daemon ok"));
        assert!(rendered.contains("stale"));
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
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                enabled_providers: vec![ProviderId::new("claude")],
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
            display_name: None,
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
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                enabled_providers: vec![],
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
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                enabled_providers: vec![ProviderId::new("codex")],
                providers,
            },
        );
        let rendered = serde_json::to_string(&status).unwrap();

        assert!(!rendered.contains(r#""provider_id":"codex""#));
    }

    fn sample_status(collected_at: DateTime<Utc>) -> StatusView {
        let account_id = AccountId::new("account");
        let account = Account {
            id: account_id.clone(),
            provider_id: ProviderId::new("claude"),
            external_account_id: "joey".to_string(),
            display_name: Some("Claude team".to_string()),
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
            config_path: "/tmp/config.json".to_string(),
            socket_path: "/tmp/usage.sock".to_string(),
            db_path: "/tmp/usage.sqlite3".to_string(),
            enabled_providers: vec![ProviderId::new("claude")],
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
