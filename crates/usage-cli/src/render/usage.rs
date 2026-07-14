use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

use chrono::{DateTime, Days, Local, NaiveDate, TimeDelta, Utc};
use serde_json::Value;
use usage_core::{
    Account, ForecastStatus, UsageAmount, UsageDashboardSummary, UsageForecast, UsageSnapshot,
    UsageUnit, UsageWindow, UsageWindowKind,
};

use crate::{
    render::{
        labels::{identity_labels, metadata_str, plan_label},
        style::{
            format_collection_mode, format_provider_name, relative_time, truncate, visible_len,
            Theme,
        },
    },
    OutputStyle,
};

/// Fixed label column shared by the rows inside a provider panel.
const LABEL_WIDTH: usize = 8;
/// Clamp for the drawn bar so it stays legible across terminal widths.
const BAR_MIN: usize = 10;
const BAR_MAX: usize = 28;

type ForecastIndex<'a> = HashMap<(&'a str, &'a str, &'a str), &'a UsageForecast>;

#[derive(Clone, Copy)]
pub struct UsageRenderOptions {
    pub style: OutputStyle,
    pub color: bool,
    pub width: usize,
    pub details: bool,
    pub provider_scoped: bool,
}

#[cfg(test)]
pub fn render_usage(
    snapshots: &[UsageSnapshot],
    forecasts: &[UsageForecast],
    accounts: &[Account],
    style: OutputStyle,
    color: bool,
    width: usize,
    details: bool,
) -> String {
    render_usage_dashboard(
        snapshots,
        forecasts,
        accounts,
        UsageRenderOptions {
            style,
            color,
            width,
            details,
            provider_scoped: false,
        },
    )
}

#[cfg(test)]
pub fn render_usage_dashboard(
    snapshots: &[UsageSnapshot],
    forecasts: &[UsageForecast],
    accounts: &[Account],
    options: UsageRenderOptions,
) -> String {
    let UsageRenderOptions {
        style,
        color,
        width,
        details,
        provider_scoped,
    } = options;
    let dashboard = Dashboard::from_snapshots(snapshots, forecasts, accounts);
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => {
            render_dashboard(&dashboard, theme, width, details, provider_scoped)
        }
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

pub fn render_usage_dashboard_with_summary(
    snapshots: &[UsageSnapshot],
    forecasts: &[UsageForecast],
    accounts: &[Account],
    summary: &UsageDashboardSummary,
    options: UsageRenderOptions,
) -> String {
    let UsageRenderOptions {
        style,
        color,
        width,
        details,
        provider_scoped,
    } = options;
    let dashboard = Dashboard::from_summary(snapshots, forecasts, accounts, summary);
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => {
            render_dashboard(&dashboard, theme, width, details, provider_scoped)
        }
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

#[derive(Debug)]
struct Dashboard {
    overview: Overview,
    activity: Vec<ActivityDay>,
    providers: Vec<ProviderPanel>,
}

#[derive(Debug, Default)]
struct Overview {
    lifetime_tokens: u64,
    spend_usd: f64,
    peak_tokens: u64,
    current_streak_days: usize,
    longest_streak_days: usize,
}

#[derive(Debug)]
struct ActivityDay {
    date: NaiveDate,
    tokens: u64,
}

#[derive(Debug)]
struct ProviderPanel {
    provider: String,
    mode: Option<String>,
    plan: Option<String>,
    identity: Option<String>,
    session: Option<WindowLine>,
    weekly: Option<WindowLine>,
    monthly: Option<WindowLine>,
    usage: Vec<String>,
    pace: Option<PaceLine>,
    forecast: Option<ForecastStatus>,
    credits: Option<String>,
    reset_credits: Option<String>,
    extra: Vec<String>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug)]
struct WindowLine {
    label: &'static str,
    percent_remaining: Option<f64>,
    reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct PaceLine {
    status: &'static str,
    percent_used: f64,
    percent_expected: f64,
}

impl Dashboard {
    fn from_snapshots(
        snapshots: &[UsageSnapshot],
        forecasts: &[UsageForecast],
        accounts: &[Account],
    ) -> Self {
        let account_by_id = accounts
            .iter()
            .map(|account| (account.id.as_str(), account))
            .collect::<HashMap<_, _>>();
        let mut forecast_by_window = ForecastIndex::with_capacity(forecasts.len());
        for forecast in forecasts {
            forecast_by_window
                .entry((
                    forecast.provider_id.as_str(),
                    forecast.account_id.as_str(),
                    forecast.window_id.as_str(),
                ))
                .or_insert(forecast);
        }
        let daily_tokens = aggregate_daily_tokens(snapshots);
        let overview = Overview {
            lifetime_tokens: lifetime_tokens(snapshots),
            spend_usd: total_cost(snapshots),
            peak_tokens: daily_tokens.values().copied().max().unwrap_or_default(),
            current_streak_days: current_streak_days(&daily_tokens),
            longest_streak_days: longest_streak_days(&daily_tokens),
        };
        let activity = last_seven_days(&daily_tokens);
        let providers = snapshots
            .iter()
            .map(|snapshot| {
                ProviderPanel::from_snapshot(
                    snapshot,
                    &forecast_by_window,
                    account_by_id.get(snapshot.account_id.as_str()).copied(),
                )
            })
            .collect();

        Self {
            overview,
            activity,
            providers,
        }
    }

    fn from_summary(
        snapshots: &[UsageSnapshot],
        forecasts: &[UsageForecast],
        accounts: &[Account],
        summary: &UsageDashboardSummary,
    ) -> Self {
        let mut dashboard = Self::from_snapshots(snapshots, forecasts, accounts);
        let daily_tokens = summary
            .days
            .iter()
            .map(|day| (day.date, day.tokens))
            .collect::<BTreeMap<_, _>>();
        dashboard.activity = last_seven_days(&daily_tokens);
        let lifetime_tokens = summary
            .accounts
            .iter()
            .filter_map(|account| account.activity.as_ref()?.lifetime_tokens)
            .fold(0_u64, u64::saturating_add);
        if lifetime_tokens > 0 {
            dashboard.overview.lifetime_tokens = lifetime_tokens;
        }
        dashboard.overview.peak_tokens = daily_tokens.values().copied().max().unwrap_or_default();
        dashboard.overview.current_streak_days = current_streak_days(&daily_tokens);
        dashboard.overview.longest_streak_days = longest_streak_days(&daily_tokens);
        dashboard
    }
}

impl ProviderPanel {
    fn from_snapshot(
        snapshot: &UsageSnapshot,
        forecast_by_window: &ForecastIndex<'_>,
        account: Option<&Account>,
    ) -> Self {
        let session_window = select_window(snapshot, WindowRole::Session);
        let weekly_window = select_window(snapshot, WindowRole::Weekly);
        let monthly_window = select_window(snapshot, WindowRole::Monthly);
        let pace_window = weekly_window.or(session_window);
        let daemon_forecast = pace_window.and_then(|window| {
            forecast_by_window
                .get(&(
                    snapshot.provider_id.as_str(),
                    snapshot.account_id.as_str(),
                    window.window_id.as_str(),
                ))
                .copied()
        });
        let pace = daemon_forecast.and_then(pace_line);
        let forecast = daemon_forecast.map(|forecast| forecast.status);

        let labels = identity_labels(account, Some(snapshot));
        let selected_ids = [session_window, weekly_window, monthly_window]
            .into_iter()
            .flatten()
            .map(|window| window.window_id.as_str())
            .collect::<BTreeSet<_>>();
        Self {
            provider: format_provider_name(snapshot.provider_id.as_str()),
            mode: metadata_str(&snapshot.metadata, "collection_mode")
                .map(|mode| collection_mode_label(snapshot.provider_id.as_str(), mode)),
            plan: panel_plan(snapshot, labels.plan.as_deref()),
            identity: labels.identity,
            session: session_window.map(|window| window_line("Session", window)),
            weekly: weekly_window.map(|window| window_line("Weekly", window)),
            monthly: monthly_window.map(|window| window_line("Monthly", window)),
            usage: usage_summary(snapshot),
            pace,
            forecast,
            credits: credits_line(snapshot),
            reset_credits: reset_credits_line(snapshot),
            extra: snapshot
                .windows
                .iter()
                .filter(|window| !selected_ids.contains(window.window_id.as_str()))
                .filter(|window| !matches!(window.kind, UsageWindowKind::Credits))
                .filter(|window| !is_token_counter(window))
                .filter_map(detail_window_line)
                .collect(),
            updated_at: snapshot.collected_at,
        }
    }

    fn header_title(&self) -> String {
        let mut parts = vec![self.provider.clone()];
        if let Some(mode) = &self.mode {
            parts.push(mode.clone());
        }
        if let Some(plan) = &self.plan {
            parts.push(plan.clone());
        }
        parts.join(" · ")
    }
}

fn render_dashboard(
    dashboard: &Dashboard,
    theme: Theme,
    width: usize,
    details: bool,
    provider_scoped: bool,
) -> String {
    let mut output = String::new();
    if !provider_scoped {
        push_box(
            &mut output,
            width,
            "Overview",
            None,
            &overview_lines(&dashboard.overview, theme),
            theme,
        );
        output.push('\n');
    }
    let activity_title = if provider_scoped {
        dashboard
            .providers
            .first()
            .map(|provider| format!("{} Activity · last 7 days", provider.provider))
            .unwrap_or_else(|| "Provider Activity · last 7 days".to_string())
    } else {
        "Activity · last 7 days".to_string()
    };
    push_box(
        &mut output,
        width,
        &activity_title,
        None,
        &activity_lines(&dashboard.activity, theme, width),
        theme,
    );
    for provider in &dashboard.providers {
        output.push('\n');
        push_box(
            &mut output,
            width,
            &provider.header_title(),
            provider.identity.as_deref(),
            &provider_lines(provider, theme, width, details),
            theme,
        );
    }
    output.trim_end().to_string()
}

fn overview_lines(overview: &Overview, theme: Theme) -> Vec<String> {
    vec![
        format!(
            "{} {} {} {}",
            pad_right(theme.label("Lifetime tokens"), 16),
            pad_right(theme.value(&format_tokens(overview.lifetime_tokens)), 10),
            pad_right(theme.label("Peak tokens"), 14),
            theme.value(&format_tokens(overview.peak_tokens))
        ),
        format!(
            "{} {} {} {}",
            pad_right(theme.label("Tracked spend"), 16),
            pad_right(theme.value(&format!("${:.2}", overview.spend_usd)), 10),
            pad_right(theme.label("Current streak"), 14),
            theme.value(&format_days(overview.current_streak_days))
        ),
        format!(
            "{} {}",
            pad_right(theme.label("Longest streak"), 16),
            theme.value(&format_days(overview.longest_streak_days))
        ),
    ]
}

fn activity_lines(activity: &[ActivityDay], theme: Theme, width: usize) -> Vec<String> {
    let peak = activity
        .iter()
        .map(|day| day.tokens)
        .max()
        .unwrap_or_default();
    let bar_width = bar_width_for(width, 3 + 1 + 7 + 2);
    activity
        .iter()
        .map(|day| {
            format!(
                "{} {}  {}",
                pad_right(theme.label(&day.date.format("%a").to_string()), 3),
                pad_left(theme.value(&format_tokens(day.tokens)), 7),
                token_bar(day.tokens, peak, bar_width, theme)
            )
        })
        .collect()
}

fn provider_lines(
    provider: &ProviderPanel,
    theme: Theme,
    width: usize,
    details: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    let content_width = content_width(width);

    let windows = [&provider.session, &provider.weekly, &provider.monthly]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    lines.extend(window_rows(&windows, theme, content_width));

    if !provider.usage.is_empty() {
        let usage = provider
            .usage
            .iter()
            .map(|part| theme.value(part))
            .collect::<Vec<_>>()
            .join(&theme.muted(" · "));
        lines.push(body_line(theme, "Usage", &usage));
    }
    if let Some(pace) = &provider.pace {
        lines.push(body_line(theme, "Pace", &pace_text(pace, theme)));
    }

    if details {
        for detail in &provider.extra {
            lines.push(body_line(theme, "Detail", &theme.value(detail)));
        }
        if let Some(credits) = &provider.credits {
            lines.push(body_line(theme, "Credits", &credits_text(credits, theme)));
        }
        if let Some(reset_credits) = &provider.reset_credits {
            lines.push(body_line(theme, "Resets", &theme.good(reset_credits)));
        }
        if let Some(forecast) = &provider.forecast {
            lines.push(body_line(
                theme,
                "Forecast",
                &forecast_text(*forecast, theme),
            ));
        }
        if let Some(identity) = &provider.identity {
            lines.push(body_line(theme, "Identity", &theme.muted(identity)));
        }
    }

    lines.push(body_line(
        theme,
        "Updated",
        &theme.muted(&relative_time(provider.updated_at)),
    ));
    lines
}

/// `label`-padded key/value row used throughout a provider panel body.
fn body_line(theme: Theme, label: &str, value: &str) -> String {
    format!("{}  {value}", pad_right(theme.label(label), LABEL_WIDTH))
}

/// Render the session/weekly/monthly rows with a shared bar width and a
/// right-padded reset column so every row lines up inside the panel.
fn window_rows(windows: &[&WindowLine], theme: Theme, content_width: usize) -> Vec<String> {
    let resets = windows
        .iter()
        .map(|window| {
            window
                .reset_at
                .map(reset_label)
                .unwrap_or_else(|| "reset unknown".to_string())
        })
        .collect::<Vec<_>>();
    let reset_col = resets.iter().map(|reset| reset.len()).max().unwrap_or(0);
    // label + "  " + bar + "  " + pct-and-direction(9) + "  ·  " + reset
    let fixed = LABEL_WIDTH + 2 + 2 + 9 + 5;
    let bar_width = content_width
        .saturating_sub(fixed + reset_col)
        .clamp(BAR_MIN, BAR_MAX);

    windows
        .iter()
        .zip(resets)
        .map(|(window, reset)| {
            let percent = window
                .percent_remaining
                .map(|value| format!("{} left", format_percent(value)))
                .unwrap_or_else(|| "?".to_string());
            let percent = window
                .percent_remaining
                .map(|value| theme.quota(value, &percent))
                .unwrap_or_else(|| theme.muted(&percent));
            let bar = window
                .percent_remaining
                .map(|value| percent_bar(value, bar_width, theme))
                .unwrap_or_else(|| theme.muted(&"░".repeat(bar_width)));
            format!(
                "{}  {}  {}  {}  {}",
                pad_right(theme.label(window.label), LABEL_WIDTH),
                bar,
                pad_left(percent, 4),
                theme.muted("·"),
                theme.muted(&reset)
            )
        })
        .collect()
}

fn push_box(
    output: &mut String,
    width: usize,
    title: &str,
    identity: Option<&str>,
    lines: &[String],
    theme: Theme,
) {
    push_top_border(output, width, title, identity, theme);
    for line in lines {
        push_content_line(output, width, line, theme);
    }
    push_bottom_border(output, width, theme);
}

fn push_top_border(
    output: &mut String,
    width: usize,
    title: &str,
    identity: Option<&str>,
    theme: Theme,
) {
    // ╭─ {title} ──…── {identity} ─╮   (identity optional, right-aligned)
    let identity = identity
        .map(|identity| truncate(identity, width.saturating_sub(20)))
        .filter(|identity| !identity.is_empty());
    let identity_len = identity
        .as_ref()
        .map(|identity| visible_len(identity) + 3)
        .unwrap_or(1);
    let max_title = width.saturating_sub(2 + 3 + identity_len + 1);
    let title = truncate(title, max_title);
    let fill = width
        .saturating_sub(2 + 3 + visible_len(&title) + identity_len)
        .max(1);

    output.push_str(&theme.border("╭─ "));
    output.push_str(&theme.title(&title));
    output.push(' ');
    output.push_str(&theme.border(&"─".repeat(fill)));
    match identity {
        Some(identity) => {
            output.push_str(&theme.border(" "));
            output.push_str(&theme.muted(&identity));
            output.push_str(&theme.border(" ─╮"));
        }
        None => output.push_str(&theme.border("─╮")),
    }
    output.push('\n');
}

fn push_content_line(output: &mut String, width: usize, content: &str, theme: Theme) {
    let inner_width = width - 4;
    let content = truncate(content, inner_width);
    let padding = inner_width.saturating_sub(visible_len(&content));
    let _ = writeln!(
        output,
        "{} {content}{} {}",
        theme.border("│"),
        " ".repeat(padding),
        theme.border("│")
    );
}

fn push_bottom_border(output: &mut String, width: usize, theme: Theme) {
    output.push_str(&theme.border("╰"));
    output.push_str(&theme.border(&"─".repeat(width - 2)));
    output.push_str(&theme.border("╯"));
    output.push('\n');
}

/// Visible width available inside a box (between the `│ ` … ` │` frame).
fn content_width(width: usize) -> usize {
    width.saturating_sub(4)
}

/// Bar width that fills the space left after `reserved` fixed columns, clamped
/// to a legible range.
fn bar_width_for(width: usize, reserved: usize) -> usize {
    content_width(width)
        .saturating_sub(reserved)
        .clamp(BAR_MIN, BAR_MAX)
}

fn window_line(label: &'static str, window: &UsageWindow) -> WindowLine {
    WindowLine {
        label,
        percent_remaining: percent_remaining(window),
        reset_at: window.reset_at,
    }
}

/// Token counters (today / 30-day / lifetime) condensed into one line.
fn usage_summary(snapshot: &UsageSnapshot) -> Vec<String> {
    snapshot
        .windows
        .iter()
        .filter(|window| is_token_counter(window))
        .filter_map(|window| {
            let amount = window.used.as_ref().or(window.remaining.as_ref())?;
            Some(format!(
                "{} {}",
                format_amount(amount),
                token_period(&window.label)
            ))
        })
        .collect()
}

fn is_token_counter(window: &UsageWindow) -> bool {
    window.used.is_some()
        && window.percent_remaining.is_none()
        && window.percent_used.is_none()
        && window.label.to_ascii_lowercase().contains("token")
}

fn token_period(label: &str) -> String {
    let lower = label.to_ascii_lowercase();
    if lower.contains("today") {
        "today".to_string()
    } else if lower.contains("lifetime") {
        "lifetime".to_string()
    } else if lower.contains("30") || lower.contains("month") {
        "30d".to_string()
    } else {
        label
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_ascii_lowercase()
    }
}

fn credits_line(snapshot: &UsageSnapshot) -> Option<String> {
    let credit = snapshot
        .windows
        .iter()
        .filter(|window| matches!(window.kind, UsageWindowKind::Credits))
        .find(|window| window.remaining.is_some())
        .or_else(|| {
            snapshot
                .windows
                .iter()
                .find(|window| window.window_id == "codex_credits")
        })?;

    if let Some(remaining) = &credit.remaining {
        let value = format_amount(remaining);
        let state = if remaining.value <= 0.0 {
            "empty"
        } else {
            "available"
        };
        Some(format!("{:<9} {state}", format!("{value} left")))
    } else if let (Some(used), Some(limit)) = (&credit.used, &credit.limit) {
        Some(format!(
            "{} used of {}",
            format_amount(used),
            format_amount(limit)
        ))
    } else {
        None
    }
}

fn reset_credits_line(snapshot: &UsageSnapshot) -> Option<String> {
    let metadata = snapshot.metadata.get("rate_limit_reset_credits")?;
    let available = metadata
        .get("available_count")
        .and_then(f64_value)
        .or_else(|| {
            snapshot
                .metadata
                .get("rate_limit_reset_credits_available_count")
                .and_then(f64_value)
        })?;
    if available <= 0.0 {
        return None;
    }

    let count = available.round() as u64;
    let next_expires_at = metadata
        .get("next_expires_at")
        .and_then(f64_value)
        .and_then(|seconds| DateTime::from_timestamp(seconds.round() as i64, 0));
    let suffix = next_expires_at
        .map(reset_credit_expiry_label)
        .unwrap_or_else(|| "expiry unknown".to_string());

    Some(format!(
        "{} available  {}",
        pluralize(count, "reset", "resets"),
        suffix
    ))
}

fn pace_line(forecast: &UsageForecast) -> Option<PaceLine> {
    let percent_expected = forecast.expected_percent_used?;
    let delta = forecast.pace_delta_percent?;
    let status = if delta > 5.0 {
        "over"
    } else if delta < -5.0 {
        "under"
    } else {
        "on track"
    };
    Some(PaceLine {
        status,
        percent_used: forecast.current_percent_used,
        percent_expected,
    })
}

fn pace_text(pace: &PaceLine, theme: Theme) -> String {
    format!(
        "{} {} {} used vs {} expected",
        theme.pace(pace.status),
        theme.muted("·"),
        theme.value(&format!("{:.0}%", pace.percent_used)),
        theme.value(&format!("{:.0}%", pace.percent_expected))
    )
}

#[derive(Clone, Copy)]
enum WindowRole {
    Session,
    Weekly,
    Monthly,
}

fn select_window(snapshot: &UsageSnapshot, role: WindowRole) -> Option<&UsageWindow> {
    snapshot
        .windows
        .iter()
        .filter(|window| percent_remaining(window).is_some())
        .filter(|window| role_matches(window, role))
        .min_by_key(|window| {
            if window.window_id.contains("additional") {
                1
            } else {
                0
            }
        })
}

fn role_matches(window: &UsageWindow, role: WindowRole) -> bool {
    let name = format!(
        "{} {}",
        window.window_id.to_ascii_lowercase(),
        window.label.to_ascii_lowercase()
    );
    match role {
        WindowRole::Session => {
            matches!(window.kind, UsageWindowKind::Session)
                || name.contains("session")
                || name.contains("five_hour")
                || name.contains("five hour")
        }
        WindowRole::Weekly => {
            matches!(window.kind, UsageWindowKind::Weekly)
                || name.contains("weekly")
                || name.contains("seven_day")
                || name.contains("seven day")
        }
        WindowRole::Monthly => {
            matches!(window.kind, UsageWindowKind::Monthly)
                || name.contains("monthly")
                || name.contains("30d")
                || name.contains("30 days")
        }
    }
}

/// Plan label for the panel header, preferring snapshot metadata over the
/// account-derived fallback.
fn panel_plan(snapshot: &UsageSnapshot, fallback_plan: Option<&str>) -> Option<String> {
    metadata_str(&snapshot.metadata, "plan_type")
        .or_else(|| metadata_str(&snapshot.metadata, "subscription_type"))
        .map(plan_label)
        .or_else(|| fallback_plan.map(str::to_string))
}

fn detail_window_line(window: &UsageWindow) -> Option<String> {
    let value = if let Some(remaining) = &window.remaining {
        format!("{} remaining", format_amount(remaining))
    } else if let (Some(used), Some(limit)) = (&window.used, &window.limit) {
        format!("{} / {} used", format_amount(used), format_amount(limit))
    } else if let Some(used) = &window.used {
        format!("{} used", format_amount(used))
    } else if let Some(percent) = percent_remaining(window) {
        format!("{} left", format_percent(percent))
    } else {
        return None;
    };
    let reset = window
        .reset_at
        .map(|reset| format!(" · {}", reset_label(reset)))
        .unwrap_or_default();
    Some(format!("{}: {value}{reset}", window.label))
}

fn collection_mode_label(provider_id: &str, mode: &str) -> String {
    format_collection_mode(provider_id, mode)
}

fn aggregate_daily_tokens(snapshots: &[UsageSnapshot]) -> BTreeMap<NaiveDate, u64> {
    let mut days = BTreeMap::new();
    for snapshot in snapshots {
        for (date, tokens) in daily_rows(snapshot) {
            *days.entry(date).or_insert(0) += tokens;
        }
    }
    days
}

fn lifetime_tokens(snapshots: &[UsageSnapshot]) -> u64 {
    snapshots
        .iter()
        .filter_map(activity_metadata)
        .filter_map(|activity| {
            u64_field(activity, "lifetime_tokens")
                .or_else(|| u64_field(activity, "total_tokens"))
                .or_else(|| u64_field(activity, "lookback_tokens"))
        })
        .sum()
}

fn total_cost(snapshots: &[UsageSnapshot]) -> f64 {
    snapshots
        .iter()
        .filter_map(cost_metadata)
        .filter_map(|cost| {
            cost.get("total_cost_usd").and_then(f64_value).or_else(|| {
                cost.get("by_day").and_then(Value::as_array).map(|rows| {
                    rows.iter()
                        .filter_map(|row| row.get("cost_usd").and_then(f64_value))
                        .sum()
                })
            })
        })
        .sum()
}

fn daily_rows(snapshot: &UsageSnapshot) -> Vec<(NaiveDate, u64)> {
    let Some(cost) = activity_metadata(snapshot) else {
        return Vec::new();
    };
    cost.get("by_day")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let date = row.get("date").and_then(Value::as_str)?;
            let tokens = row.get("tokens").and_then(u64_value)?;
            NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .ok()
                .map(|date| (date, tokens))
        })
        .collect()
}

fn cost_metadata(snapshot: &UsageSnapshot) -> Option<&Value> {
    let provider_key = format!("{}_cost", snapshot.provider_id.as_str());
    snapshot.metadata.get(&provider_key).or_else(|| {
        snapshot
            .metadata
            .as_object()?
            .values()
            .find(|value| value.get("by_day").is_some() && value.get("total_tokens").is_some())
    })
}

fn activity_metadata(snapshot: &UsageSnapshot) -> Option<&Value> {
    let provider_key = format!("{}_activity", snapshot.provider_id.as_str());
    snapshot
        .metadata
        .get(&provider_key)
        .or_else(|| cost_metadata(snapshot))
}

fn last_seven_days(daily_tokens: &BTreeMap<NaiveDate, u64>) -> Vec<ActivityDay> {
    let today = Local::now().date_naive();
    let latest = daily_tokens.keys().next_back().copied().unwrap_or(today);
    let end = latest.max(today);
    let start = end.checked_sub_days(Days::new(6)).unwrap_or(end);
    (0..7)
        .filter_map(|offset| start.checked_add_days(Days::new(offset)))
        .map(|date| ActivityDay {
            date,
            tokens: daily_tokens.get(&date).copied().unwrap_or_default(),
        })
        .collect()
}

fn current_streak_days(daily_tokens: &BTreeMap<NaiveDate, u64>) -> usize {
    let active = active_days(daily_tokens);
    let Some(mut cursor) = active.iter().next_back().copied() else {
        return 0;
    };
    let mut count = 0;
    while active.contains(&cursor) {
        count += 1;
        let Some(previous) = cursor.checked_sub_days(Days::new(1)) else {
            break;
        };
        cursor = previous;
    }
    count
}

fn longest_streak_days(daily_tokens: &BTreeMap<NaiveDate, u64>) -> usize {
    let mut longest = 0;
    let mut current = 0;
    let mut previous = None;
    for date in active_days(daily_tokens) {
        if previous.and_then(|previous: NaiveDate| previous.checked_add_days(Days::new(1)))
            == Some(date)
        {
            current += 1;
        } else {
            current = 1;
        }
        longest = longest.max(current);
        previous = Some(date);
    }
    longest
}

fn active_days(daily_tokens: &BTreeMap<NaiveDate, u64>) -> BTreeSet<NaiveDate> {
    daily_tokens
        .iter()
        .filter_map(|(date, tokens)| (*tokens > 0).then_some(*date))
        .collect()
}

fn percent_remaining(window: &UsageWindow) -> Option<f64> {
    window
        .percent_remaining
        .or_else(|| window.percent_used.map(|value| 100.0 - value))
        .map(|value| value.clamp(0.0, 100.0))
}

fn reset_label(reset_at: DateTime<Utc>) -> String {
    let now = Utc::now();
    if reset_at <= now {
        return "reset passed".to_string();
    }

    let delta = reset_at - now;
    if delta.num_minutes() < 60 {
        return format!("resets in {}m", delta.num_minutes().max(1));
    }
    if delta.num_hours() < 6 {
        let hours = delta.num_hours();
        let minutes = (delta - TimeDelta::hours(hours)).num_minutes();
        return format!("resets in {hours}h {minutes}m");
    }
    if delta.num_hours() < 24
        && reset_at.with_timezone(&Local).date_naive() == Local::now().date_naive()
    {
        return format!(
            "resets {}",
            reset_at.with_timezone(&Local).format("%-I:%M %p")
        );
    }
    if delta.num_days() < 7 {
        let days = delta.num_days();
        let hours = (delta - TimeDelta::days(days)).num_hours();
        return format!("resets in {days}d {hours}h");
    }
    format!("resets {}", reset_at.with_timezone(&Local).format("%b %-d"))
}

fn reset_credit_expiry_label(expires_at: DateTime<Utc>) -> String {
    let now = Utc::now();
    if expires_at <= now {
        return "expired".to_string();
    }

    let local = expires_at.with_timezone(&Local);
    format!("expires {}", local.format("%a, %b %-d at %-I:%M %p"))
}

fn pluralize(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn token_bar(tokens: u64, peak: u64, width: usize, theme: Theme) -> String {
    if peak == 0 {
        return theme.muted(&"░".repeat(width));
    }
    let filled = ((tokens as f64 / peak as f64) * width as f64).round() as usize;
    let filled = filled.clamp((tokens > 0) as usize, width);
    bar_segments(filled, width, |value| theme.accent(value), theme)
}

fn percent_bar(percent: f64, width: usize, theme: Theme) -> String {
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    bar_segments(filled, width, |value| theme.quota(percent, value), theme)
}

fn bar_segments(
    filled: usize,
    width: usize,
    filled_style: impl FnOnce(&str) -> String,
    theme: Theme,
) -> String {
    let empty = width.saturating_sub(filled);
    format!(
        "{}{}",
        filled_style(&"█".repeat(filled)),
        theme.muted(&"░".repeat(empty))
    )
}

fn forecast_text(status: ForecastStatus, theme: Theme) -> String {
    match status {
        ForecastStatus::Safe | ForecastStatus::OnPace => theme.good("on track to last until reset"),
        ForecastStatus::AtRisk => theme.warn("at risk before reset"),
        ForecastStatus::Exhausted => theme.danger("exhausted before reset"),
        ForecastStatus::InsufficientData => theme.muted("insufficient data"),
    }
}

fn credits_text(credits: &str, theme: Theme) -> String {
    if credits.contains("empty") {
        theme.danger(credits)
    } else if credits.contains("available") {
        theme.good(credits)
    } else {
        theme.value(credits)
    }
}

fn pad_right(value: String, width: usize) -> String {
    let padding = width.saturating_sub(visible_len(&value));
    format!("{value}{}", " ".repeat(padding))
}

fn pad_left(value: String, width: usize) -> String {
    let padding = width.saturating_sub(visible_len(&value));
    format!("{}{value}", " ".repeat(padding))
}

fn format_tokens(tokens: u64) -> String {
    format_scaled(
        tokens as f64,
        &[("B", 1_000_000_000.0), ("M", 1_000_000.0), ("K", 1_000.0)],
    )
}

fn format_scaled(value: f64, units: &[(&str, f64)]) -> String {
    for (suffix, divisor) in units {
        if value >= *divisor {
            return format_compact_number(value / divisor, suffix);
        }
    }
    format!("{value:.0}")
}

fn format_compact_number(value: f64, suffix: &str) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{rounded:.0}{suffix}")
    } else {
        format!("{rounded:.1}{suffix}")
    }
}

fn format_percent(value: f64) -> String {
    format!("{:.0}%", value.clamp(0.0, 100.0).round())
}

fn format_days(days: usize) -> String {
    match days {
        0 => "0 days".to_string(),
        1 => "1 day".to_string(),
        value => format!("{value} days"),
    }
}

fn format_amount(amount: &UsageAmount) -> String {
    match amount.unit {
        UsageUnit::Tokens => format_tokens(amount.value.max(0.0).round() as u64),
        UsageUnit::Usd => format!("${:.2}", amount.value),
        UsageUnit::Percent => format_percent(amount.value),
        UsageUnit::Credits => format_compact_number(amount.value, ""),
        UsageUnit::Requests | UsageUnit::Unknown => format_compact_number(amount.value, ""),
    }
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(u64_value)
}

fn u64_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|value| value.round() as u64))
        .or_else(|| value.as_str()?.parse().ok())
}

fn f64_value(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::style::strip_ansi;
    use chrono::TimeZone;
    use serde_json::json;
    use usage_core::{AccountId, ForecastConfidence, ProviderId};

    const TEST_WIDTH: usize = 80;

    #[test]
    fn renders_dashboard_sections() {
        let (snapshot, account) = sample_dashboard();

        let rendered = render_usage_dashboard(
            &[snapshot],
            &[],
            &[account],
            UsageRenderOptions {
                style: OutputStyle::Dashboard,
                color: false,
                width: TEST_WIDTH,
                details: false,
                provider_scoped: false,
            },
        );

        assert!(rendered.contains("Overview"));
        assert!(!rendered.contains("Coverage"));
        assert!(rendered.contains("Activity · last 7 days"));
        assert!(rendered.contains("Codex · openai-web · Pro Lite"));
        assert!(rendered.contains("Session"));
        assert!(rendered.contains("Monthly"));
        assert!(rendered.contains("Updated"));
        assert!(!rendered.contains("\x1b["));
    }

    #[test]
    fn dashboards_omit_aggregate_provenance_warning() {
        let (snapshot, account) = sample_dashboard();
        let mut summary = usage_core::aggregate_usage_dashboard(Vec::new());
        summary.provenance.mixed_scope = true;
        summary.provenance.explanation = "aggregate provenance warning".to_string();

        let aggregate = render_usage_dashboard_with_summary(
            std::slice::from_ref(&snapshot),
            &[],
            std::slice::from_ref(&account),
            &summary,
            UsageRenderOptions {
                style: OutputStyle::Dashboard,
                color: false,
                width: TEST_WIDTH,
                details: false,
                provider_scoped: false,
            },
        );
        let provider = render_usage_dashboard_with_summary(
            &[snapshot],
            &[],
            &[account],
            &summary,
            UsageRenderOptions {
                style: OutputStyle::Dashboard,
                color: false,
                width: TEST_WIDTH,
                details: false,
                provider_scoped: true,
            },
        );

        assert!(aggregate.contains("Overview"));
        assert!(!aggregate.contains("aggregate provenance warning"));
        assert!(!provider.contains("Overview"));
        assert!(!provider.contains("aggregate provenance warning"));
        assert!(provider.starts_with("╭─ Codex Activity · last 7 days"));
        assert!(provider.contains("Codex · openai-web · Pro Lite"));
    }

    #[test]
    fn details_flag_reveals_extra_rows() {
        let (snapshot, account) = sample_dashboard();

        let focused = render_usage(
            std::slice::from_ref(&snapshot),
            &[],
            std::slice::from_ref(&account),
            OutputStyle::Dashboard,
            false,
            TEST_WIDTH,
            false,
        );
        // Identity moves to the panel header; there is no body "Identity" row
        // and no reset-credits line in the focused view.
        assert!(!focused.contains("Identity"));
        assert!(!focused.contains("Resets"));

        let detailed = render_usage(
            &[snapshot],
            &[],
            &[account],
            OutputStyle::Dashboard,
            false,
            TEST_WIDTH,
            true,
        );
        assert!(detailed.contains("Identity"));
        assert!(detailed.contains("user@example.com"));
        assert!(detailed.contains("2 resets available"));
    }

    #[test]
    fn colored_dashboard_keeps_box_widths() {
        let (mut snapshot, account) = sample_dashboard();
        snapshot.windows[0].percent_used = Some(92.0);
        snapshot.windows[0].percent_remaining = Some(8.0);

        for width in [62, 100] {
            let rendered = render_usage(
                std::slice::from_ref(&snapshot),
                &[],
                std::slice::from_ref(&account),
                OutputStyle::Dashboard,
                true,
                width,
                true,
            );

            assert!(rendered.contains("\x1b["));
            assert!(strip_ansi(&rendered).contains("Codex · openai-web · Pro Lite"));
            for line in rendered.lines().filter(|line| !line.is_empty()) {
                assert_eq!(visible_len(line), width, "line: {line:?}");
            }
        }
    }

    #[test]
    fn renders_daemon_generated_pace_and_forecast() {
        let (snapshot, account) = sample_dashboard();
        let forecast = UsageForecast {
            provider_id: snapshot.provider_id.clone(),
            account_id: snapshot.account_id.clone(),
            window_id: "codex_weekly".to_string(),
            generated_at: Utc::now(),
            reset_at: snapshot.windows[1].reset_at,
            current_percent_used: 40.0,
            expected_percent_used: Some(30.0),
            pace_delta_percent: Some(10.0),
            rate_percent_per_hour: Some(2.0),
            projected_percent_at_reset: Some(110.0),
            projected_percent_remaining_at_reset: Some(0.0),
            predicted_exhaustion_at: Some(Utc::now() + TimeDelta::days(2)),
            status: ForecastStatus::AtRisk,
            sample_count: 12,
            confidence: ForecastConfidence::High,
        };

        let rendered = render_usage(
            &[snapshot],
            &[forecast],
            &[account],
            OutputStyle::Dashboard,
            false,
            TEST_WIDTH,
            true,
        );

        assert!(rendered.contains("Pace"));
        assert!(rendered.contains("over"));
        assert!(rendered.contains("at risk before reset"));
    }

    #[test]
    fn duplicate_forecast_keys_keep_the_first_forecast() {
        let (snapshot, account) = sample_dashboard();
        let first = UsageForecast {
            provider_id: snapshot.provider_id.clone(),
            account_id: snapshot.account_id.clone(),
            window_id: "codex_weekly".to_string(),
            generated_at: Utc::now(),
            reset_at: snapshot.windows[1].reset_at,
            current_percent_used: 40.0,
            expected_percent_used: Some(40.0),
            pace_delta_percent: Some(0.0),
            rate_percent_per_hour: Some(1.0),
            projected_percent_at_reset: Some(80.0),
            projected_percent_remaining_at_reset: Some(20.0),
            predicted_exhaustion_at: None,
            status: ForecastStatus::Safe,
            sample_count: 12,
            confidence: ForecastConfidence::High,
        };
        let mut duplicate = first.clone();
        duplicate.pace_delta_percent = Some(50.0);
        duplicate.status = ForecastStatus::AtRisk;

        let dashboard = Dashboard::from_snapshots(
            std::slice::from_ref(&snapshot),
            &[first, duplicate],
            std::slice::from_ref(&account),
        );

        assert_eq!(dashboard.providers[0].forecast, Some(ForecastStatus::Safe));
        assert_eq!(
            dashboard.providers[0].pace.as_ref().map(|pace| pace.status),
            Some("on track")
        );
    }

    #[test]
    fn labels_claude_cli_collection_as_terminal() {
        assert_eq!(
            collection_mode_label("claude", "claude_cli_usage"),
            "terminal"
        );
    }

    fn sample_dashboard() -> (UsageSnapshot, Account) {
        let account_id = AccountId::new("account");
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: account_id.clone(),
            collected_at: Utc::now(),
            windows: vec![
                UsageWindow {
                    window_id: "codex_session".to_string(),
                    label: "Codex session".to_string(),
                    kind: UsageWindowKind::Session,
                    used: None,
                    limit: None,
                    remaining: None,
                    percent_used: Some(25.0),
                    percent_remaining: Some(75.0),
                    reset_at: Some(Utc::now() + TimeDelta::hours(2)),
                },
                UsageWindow {
                    window_id: "codex_weekly".to_string(),
                    label: "Codex weekly".to_string(),
                    kind: UsageWindowKind::Weekly,
                    used: None,
                    limit: None,
                    remaining: None,
                    percent_used: Some(40.0),
                    percent_remaining: Some(60.0),
                    reset_at: Some(Utc::now() + TimeDelta::days(3)),
                },
                UsageWindow {
                    window_id: "codex_monthly".to_string(),
                    label: "Codex monthly".to_string(),
                    kind: UsageWindowKind::Monthly,
                    used: None,
                    limit: None,
                    remaining: None,
                    percent_used: Some(50.0),
                    percent_remaining: Some(50.0),
                    reset_at: Some(Utc::now() + TimeDelta::days(20)),
                },
            ],
            metadata: json!({
                "collection_mode": "wham_usage_api",
                "plan_type": "prolite",
                "email": "user@example.com",
                "rate_limit_reset_credits_available_count": 2,
                "rate_limit_reset_credits": {
                    "available_count": 2,
                    "next_expires_at": (Utc::now() + TimeDelta::days(2)).timestamp()
                },
                "codex_cost": {
                    "total_tokens": 1_250_000_000_u64,
                    "by_day": [
                        {"date": "2026-07-05", "tokens": 12_000_000_u64},
                        {"date": "2026-07-06", "tokens": 44_000_000_u64}
                    ]
                }
            }),
        };
        let account = Account {
            id: account_id,
            provider_id: ProviderId::new("codex"),
            external_account_id: "external".to_string(),
            profile_id: None,
            display_name: Some("Codex".to_string()),
            display_name_source: Default::default(),
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        };

        (snapshot, account)
    }
}
