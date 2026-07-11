use std::fmt::Write;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use usage_core::{
    Account, AddProviderAccountResponse, ConfigResponse, ProviderActionResponse,
    ProviderRefreshResult, ProviderSetupResponse, UsageSnapshot,
};

use crate::{
    render::{
        labels::{identity_labels, latest_snapshots_by_account},
        style::{
            format_collection_mode, format_local_time, format_provider_name, relative_time,
            relative_time_opt, Theme,
        },
        table::Table,
    },
    OutputStyle,
};

pub fn render_accounts(
    accounts: &[Account],
    snapshots: &[UsageSnapshot],
    style: OutputStyle,
    color: bool,
    verbose: bool,
) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => render_accounts_dashboard(accounts, snapshots, theme, verbose),
        OutputStyle::Compact => render_accounts_compact(accounts, snapshots, theme, verbose),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

pub fn render_config(config: &ConfigResponse, style: OutputStyle, color: bool) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => render_config_dashboard(config, theme),
        OutputStyle::Compact => render_config_compact(config, theme),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

pub fn render_refresh(
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    provider_results: &[ProviderRefreshResult],
    accounts: &[Account],
    snapshots: &[UsageSnapshot],
    style: OutputStyle,
    color: bool,
) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => render_refresh_dashboard(
            started_at,
            finished_at,
            provider_results,
            accounts,
            snapshots,
            theme,
        ),
        OutputStyle::Compact => render_refresh_compact(
            started_at,
            finished_at,
            provider_results,
            accounts,
            snapshots,
            theme,
        ),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

fn render_refresh_dashboard(
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    provider_results: &[ProviderRefreshResult],
    accounts: &[Account],
    snapshots: &[UsageSnapshot],
    theme: Theme,
) -> String {
    let account_by_id = account_by_id(accounts);
    let latest_by_account = latest_snapshots_by_account(snapshots);
    let mut output = String::new();
    let _ = writeln!(output, "{}", theme.title("Refresh complete"));
    output.push('\n');
    push_kv(
        &mut output,
        theme,
        "Started",
        &format_local_time(Some(started_at)),
    );
    push_kv(
        &mut output,
        theme,
        "Finished",
        &format_local_time(Some(finished_at)),
    );
    push_kv(
        &mut output,
        theme,
        "Duration",
        &format_duration(finished_at - started_at),
    );

    output.push('\n');
    let mut table = Table::new([
        "Provider",
        "Identity",
        "Plan",
        "Status",
        "Mode",
        "Collected",
        "Message",
    ]);
    for result in provider_results {
        let account = result
            .account_id
            .as_ref()
            .and_then(|id| account_by_id.get(id.as_str()).copied());
        let snapshot = result
            .account_id
            .as_ref()
            .and_then(|id| latest_by_account.get(id.as_str()).copied());
        let labels = identity_labels(account, snapshot);
        table.row([
            format_provider_name(result.provider_id.as_str()),
            labels.identity.unwrap_or_else(|| "-".to_string()),
            labels.plan.unwrap_or_else(|| "-".to_string()),
            theme.status(&json_name(&result.status)),
            result
                .collection_mode
                .as_deref()
                .map(|mode| format_collection_mode(result.provider_id.as_str(), mode))
                .unwrap_or_else(|| "-".to_string()),
            relative_time_opt(result.collected_at),
            result.message.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }

    output.push_str(&table.render(theme));
    output.trim_end().to_string()
}

fn render_refresh_compact(
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    provider_results: &[ProviderRefreshResult],
    accounts: &[Account],
    snapshots: &[UsageSnapshot],
    theme: Theme,
) -> String {
    let account_by_id = account_by_id(accounts);
    let latest_by_account = latest_snapshots_by_account(snapshots);
    let mut lines = vec![format!(
        "{} in {}",
        theme.title("Refresh complete"),
        format_duration(finished_at - started_at)
    )];
    lines.extend(provider_results.iter().map(|result| {
        let labels = result
            .account_id
            .as_ref()
            .map(|id| {
                identity_labels(
                    account_by_id.get(id.as_str()).copied(),
                    latest_by_account.get(id.as_str()).copied(),
                )
            })
            .unwrap_or_default();
        let identity = labels
            .identity
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        let plan = labels
            .plan
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        let mode = result
            .collection_mode
            .as_deref()
            .map(|mode| {
                format!(
                    " · {}",
                    format_collection_mode(result.provider_id.as_str(), mode)
                )
            })
            .unwrap_or_default();
        let message = result
            .message
            .as_ref()
            .map(|message| format!(" · {message}"))
            .unwrap_or_default();
        format!(
            "{}{}{}: {}{} · collected {}{}",
            theme.title(&format_provider_name(result.provider_id.as_str())),
            identity,
            plan,
            theme.status(&json_name(&result.status)),
            mode,
            relative_time_opt(result.collected_at),
            message
        )
    }));
    lines.join("\n")
}

fn render_accounts_dashboard(
    accounts: &[Account],
    snapshots: &[UsageSnapshot],
    theme: Theme,
    verbose: bool,
) -> String {
    let latest_by_account = latest_snapshots_by_account(snapshots);
    let mut output = String::new();
    let _ = writeln!(output, "{}", theme.title("Accounts"));
    output.push('\n');

    let mut headers = vec!["Provider", "Identity", "Account ID", "State", "Plan"];
    if verbose {
        headers.extend(["Profile", "External ID"]);
    }
    headers.push("Updated");
    let mut table = Table::new(headers);
    for account in accounts {
        let labels = identity_labels(
            Some(account),
            latest_by_account.get(account.id.as_str()).copied(),
        );
        let mut row = vec![
            format_provider_name(account.provider_id.as_str()),
            labels.identity.unwrap_or_else(|| "-".to_string()),
            account.id.as_str().to_string(),
            account_state(account, theme),
            labels.plan.unwrap_or_else(|| "-".to_string()),
        ];
        if verbose {
            row.push(
                account
                    .profile_id
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            );
            row.push(account.external_account_id.clone());
        }
        row.push(relative_time(account.updated_at));
        table.row(row);
    }

    output.push_str(&table.render(theme));
    output.trim_end().to_string()
}

fn render_accounts_compact(
    accounts: &[Account],
    snapshots: &[UsageSnapshot],
    theme: Theme,
    verbose: bool,
) -> String {
    let latest_by_account = latest_snapshots_by_account(snapshots);
    accounts
        .iter()
        .map(|account| {
            let labels = identity_labels(
                Some(account),
                latest_by_account.get(account.id.as_str()).copied(),
            );
            let mut parts = Vec::new();
            if let Some(identity) = labels.identity {
                let kind = labels.identity_kind.unwrap_or("identity");
                parts.push(format!("{kind} {identity}"));
            }
            if let Some(plan) = labels.plan {
                parts.push(format!("plan {plan}"));
            }
            parts.push(format!("id {}", account.id));
            parts.push(format!("state {}", account_state_plain(account)));
            if verbose {
                if let Some(profile) = &account.profile_id {
                    parts.push(format!("profile {profile}"));
                }
                parts.push(format!("external {}", account.external_account_id));
            }
            parts.push(format!("updated {}", relative_time(account.updated_at)));
            format!(
                "{}: {}",
                theme.title(&format_provider_name(account.provider_id.as_str())),
                parts.join(" · ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_account_action(account: &Account, style: OutputStyle, color: bool) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => {
            let mut output = String::new();
            let _ = writeln!(output, "{}", theme.title("Account updated"));
            output.push('\n');
            push_kv(
                &mut output,
                theme,
                "Provider",
                &format_provider_name(account.provider_id.as_str()),
            );
            push_kv(&mut output, theme, "Account ID", account.id.as_str());
            push_kv(
                &mut output,
                theme,
                "Name",
                account.display_name.as_deref().unwrap_or("-"),
            );
            push_kv(
                &mut output,
                theme,
                "Email",
                account.email.as_deref().unwrap_or("-"),
            );
            push_kv(&mut output, theme, "State", account_state_plain(account));
            output.trim_end().to_string()
        }
        OutputStyle::Compact => format!(
            "{} account {} · {} · {}",
            format_provider_name(account.provider_id.as_str()),
            account.id,
            account.display_name.as_deref().unwrap_or("unnamed"),
            account_state_plain(account)
        ),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

pub fn render_added_account(
    account: &AddProviderAccountResponse,
    style: OutputStyle,
    color: bool,
) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => {
            let mut output = String::new();
            let _ = writeln!(output, "{}", theme.title("Provider account created"));
            output.push('\n');
            push_kv(
                &mut output,
                theme,
                "Provider",
                &format_provider_name(account.provider_id.as_str()),
            );
            push_kv(&mut output, theme, "Profile ID", &account.profile_id);
            push_kv(
                &mut output,
                theme,
                "Name",
                account.display_name.as_deref().unwrap_or("-"),
            );
            push_kv(&mut output, theme, "Profile path", &account.profile_path);
            let _ = writeln!(
                output,
                "\n{}",
                theme.muted("Complete sign-in in the launched provider window, then refresh.")
            );
            output.trim_end().to_string()
        }
        OutputStyle::Compact => format!(
            "Created {} profile {} · {}",
            format_provider_name(account.provider_id.as_str()),
            account.profile_id,
            account.profile_path
        ),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

pub fn render_provider_setup(
    setup: &ProviderSetupResponse,
    style: OutputStyle,
    color: bool,
) -> String {
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => {
            let mut output = String::new();
            let _ = writeln!(
                output,
                "{}",
                theme.title(&format!(
                    "{} setup",
                    format_provider_name(setup.provider_id.as_str())
                ))
            );
            output.push('\n');
            push_kv(
                &mut output,
                theme,
                "Workspace",
                setup
                    .selected_workspace_id
                    .as_deref()
                    .unwrap_or("automatic"),
            );
            if !setup.workspace_options.is_empty() {
                push_kv(
                    &mut output,
                    theme,
                    "Choices",
                    &setup.workspace_options.join(", "),
                );
            }
            if let Some(error) = &setup.discovery_error {
                push_kv(&mut output, theme, "Discovery", error);
            }
            output.push('\n');
            let mut table = Table::new(["Profile ID", "Name", "State"]);
            for profile in &setup.profiles {
                table.row([
                    profile.id.clone(),
                    profile
                        .display_name
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                    if profile.enabled {
                        theme.good("enabled")
                    } else {
                        theme.muted("disabled")
                    },
                ]);
            }
            output.push_str(&table.render(theme));
            output.trim_end().to_string()
        }
        OutputStyle::Compact => {
            let profiles = setup
                .profiles
                .iter()
                .map(|profile| {
                    format!(
                        "{}={}",
                        profile.id,
                        if profile.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} setup · workspace {} · profiles [{}]{}",
                format_provider_name(setup.provider_id.as_str()),
                setup
                    .selected_workspace_id
                    .as_deref()
                    .unwrap_or("automatic"),
                profiles,
                setup
                    .discovery_error
                    .as_ref()
                    .map(|error| format!(" · discovery {error}"))
                    .unwrap_or_default()
            )
        }
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

pub fn render_provider_action(action: &ProviderActionResponse, color: bool) -> String {
    let theme = Theme::new(color);
    format!(
        "{}: {}",
        theme.title(&format_provider_name(action.provider_id.as_str())),
        action.message
    )
}

fn render_config_dashboard(config: &ConfigResponse, theme: Theme) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "{}", theme.title("Config"));
    output.push('\n');
    push_kv(
        &mut output,
        theme,
        "Poll interval",
        &format!("{}s", config.poll_interval_seconds),
    );
    push_kv(
        &mut output,
        theme,
        "Notifications",
        if config.notifications.enabled {
            "enabled"
        } else {
            "disabled"
        },
    );
    push_kv(&mut output, theme, "Config path", &config.config_path);
    push_kv(&mut output, theme, "Socket path", &config.socket_path);
    push_kv(&mut output, theme, "Database path", &config.db_path);

    output.push('\n');
    let _ = writeln!(output, "{}", theme.title("Providers"));
    let mut table = Table::new(["Provider", "State"]);
    for (provider, toggle) in &config.providers {
        table.row([
            format_provider_name(provider),
            if toggle.enabled {
                theme.good("enabled")
            } else {
                theme.muted("disabled")
            },
        ]);
    }
    output.push_str(&table.render(theme));
    output.trim_end().to_string()
}

fn render_config_compact(config: &ConfigResponse, theme: Theme) -> String {
    let providers = config
        .providers
        .iter()
        .map(|(provider, toggle)| {
            format!(
                "{}={}",
                provider,
                if toggle.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} poll={}s · notifications={} · providers [{}] · config {} · socket {} · db {}",
        theme.title("Config"),
        config.poll_interval_seconds,
        if config.notifications.enabled {
            "enabled"
        } else {
            "disabled"
        },
        providers,
        config.config_path,
        config.socket_path,
        config.db_path
    )
}

fn push_kv(output: &mut String, theme: Theme, key: &str, value: &str) {
    let _ = writeln!(output, "{}  {}", theme.label(&format!("{key:<14}")), value);
}

fn format_duration(duration: TimeDelta) -> String {
    let seconds = duration.num_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {seconds}s");
    }

    let hours = minutes / 60;
    let minutes = minutes % 60;
    format!("{hours}h {minutes}m")
}

fn account_by_id(accounts: &[Account]) -> std::collections::HashMap<String, &Account> {
    accounts
        .iter()
        .map(|account| (account.id.as_str().to_string(), account))
        .collect()
}

fn account_state(account: &Account, theme: Theme) -> String {
    match (account.hidden, account.collection_enabled) {
        (true, false) => theme.muted("removed"),
        (true, true) => theme.muted("hidden"),
        (false, false) => theme.muted("disabled"),
        (false, true) => theme.good("active"),
    }
}

fn account_state_plain(account: &Account) -> &'static str {
    match (account.hidden, account.collection_enabled) {
        (true, false) => "removed",
        (true, true) => "hidden",
        (false, false) => "disabled",
        (false, true) => "active",
    }
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
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use usage_core::{AccountId, ProviderId, ProviderRefreshStatus, ProviderToggle, UsageWindow};

    #[test]
    fn render_accounts_dashboard_lists_display_names() {
        let rendered = render_accounts(
            &[sample_account()],
            &[sample_snapshot()],
            OutputStyle::Dashboard,
            false,
            false,
        );

        assert!(rendered.contains("Accounts"));
        assert!(rendered.contains("Team"));
        assert!(rendered.contains("joey"));
        assert!(!rendered.contains("Claude team"));
    }

    #[test]
    fn render_config_dashboard_lists_paths_and_provider_toggles() {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert("claude".to_string(), ProviderToggle { enabled: true });
        providers.insert("codex".to_string(), ProviderToggle { enabled: false });

        let rendered = render_config(
            &ConfigResponse {
                poll_interval_seconds: 1000,
                notifications: Default::default(),
                config_path: "/tmp/config.json".to_string(),
                socket_path: "/tmp/usage.sock".to_string(),
                db_path: "/tmp/usage.sqlite3".to_string(),
                enabled_providers: vec![ProviderId::new("claude")],
                providers,
            },
            OutputStyle::Dashboard,
            false,
        );

        assert!(rendered.contains("Poll interval"));
        assert!(rendered.contains("/tmp/config.json"));
        assert!(rendered.contains("Claude"));
        assert!(rendered.contains("enabled"));
        assert!(rendered.contains("Codex"));
        assert!(rendered.contains("disabled"));
    }

    #[test]
    fn render_refresh_dashboard_lists_duration_and_provider_results() {
        let rendered = render_refresh(
            Utc.with_ymd_and_hms(2026, 7, 8, 18, 5, 57).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 8, 18, 6, 4).unwrap(),
            &[sample_refresh_result(ProviderRefreshStatus::Ok, None)],
            &[sample_account()],
            &[sample_snapshot()],
            OutputStyle::Dashboard,
            false,
        );

        assert!(rendered.contains("Refresh complete"));
        assert!(rendered.contains("Duration"));
        assert!(rendered.contains("7s"));
        assert!(rendered.contains("joey"));
        assert!(rendered.contains("terminal"));
        assert!(rendered.contains("ok"));
        assert!(!rendered.contains("Claude team"));
    }

    #[test]
    fn render_refresh_compact_includes_failure_message() {
        let rendered = render_refresh(
            Utc.with_ymd_and_hms(2026, 7, 8, 18, 5, 57).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 8, 18, 6, 4).unwrap(),
            &[sample_refresh_result(
                ProviderRefreshStatus::CredentialsMissing,
                Some("Claude credentials not found".to_string()),
            )],
            &[sample_account()],
            &[sample_snapshot()],
            OutputStyle::Compact,
            false,
        );

        assert!(rendered.contains("Refresh complete in 7s"));
        assert!(rendered.contains("credentials_missing"));
        assert!(rendered.contains("Claude credentials not found"));
    }

    fn sample_account() -> Account {
        Account {
            id: AccountId::new("account"),
            provider_id: ProviderId::new("claude"),
            external_account_id: "joey".to_string(),
            profile_id: None,
            display_name: Some("Claude team".to_string()),
            display_name_source: Default::default(),
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 7, 8, 17, 18, 53).unwrap(),
        }
    }

    fn sample_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            provider_id: ProviderId::new("claude"),
            account_id: AccountId::new("account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 8, 17, 18, 55).unwrap(),
            windows: Vec::<UsageWindow>::new(),
            metadata: json!({
                "collection_mode": "claude_cli_usage",
                "credential_profile": "joey",
                "subscription_type": "team",
            }),
        }
    }

    fn sample_refresh_result(
        status: ProviderRefreshStatus,
        message: Option<String>,
    ) -> ProviderRefreshResult {
        ProviderRefreshResult {
            provider_id: ProviderId::new("claude"),
            account_id: Some(AccountId::new("account")),
            status,
            collection_mode: Some("claude_cli_usage".to_string()),
            collected_at: Some(Utc.with_ymd_and_hms(2026, 7, 8, 18, 6, 4).unwrap()),
            message,
        }
    }
}
