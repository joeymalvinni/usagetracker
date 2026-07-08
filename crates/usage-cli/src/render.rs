use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

use chrono::{DateTime, Days, Local, NaiveDate, TimeDelta, Utc};
use serde_json::Value;
use usage_core::{Account, UsageAmount, UsageSnapshot, UsageUnit, UsageWindow, UsageWindowKind};

use crate::OutputStyle;

const DASHBOARD_WIDTH: usize = 62;
const BAR_WIDTH: usize = 12;

pub fn render_usage(
    snapshots: &[UsageSnapshot],
    accounts: &[Account],
    style: OutputStyle,
    color: bool,
) -> String {
    let dashboard = Dashboard::from_snapshots(snapshots, accounts);
    let theme = Theme::new(color);
    match style {
        OutputStyle::Dashboard => render_dashboard(&dashboard, theme),
        OutputStyle::Compact => render_compact(&dashboard, theme),
        OutputStyle::Json => unreachable!("json style is handled before rendering"),
    }
}

#[derive(Clone, Copy, Debug)]
struct Theme {
    color: bool,
}

impl Theme {
    fn new(color: bool) -> Self {
        Self { color }
    }

    fn paint(self, code: &str, value: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.to_string()
        }
    }

    fn border(self, value: &str) -> String {
        self.paint("2;37", value)
    }

    fn title(self, value: &str) -> String {
        self.paint("1;97", value)
    }

    fn label(self, value: &str) -> String {
        self.paint("2;37", value)
    }

    fn value(self, value: &str) -> String {
        self.paint("1;37", value)
    }

    fn muted(self, value: &str) -> String {
        self.paint("2;37", value)
    }

    fn accent(self, value: &str) -> String {
        self.paint("36", value)
    }

    fn good(self, value: &str) -> String {
        self.paint("32", value)
    }

    fn warn(self, value: &str) -> String {
        self.paint("33", value)
    }

    fn danger(self, value: &str) -> String {
        self.paint("31", value)
    }

    fn quota(self, percent_remaining: f64, value: &str) -> String {
        if percent_remaining >= 50.0 {
            self.good(value)
        } else if percent_remaining >= 20.0 {
            self.warn(value)
        } else {
            self.danger(value)
        }
    }

    fn pace(self, status: &str) -> String {
        match status {
            "over" => self.warn(status),
            "under" | "on track" => self.good(status),
            _ => self.value(status),
        }
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
    title: String,
    session: Option<WindowLine>,
    weekly: Option<WindowLine>,
    pace: Option<PaceLine>,
    forecast: Option<String>,
    credits: Option<String>,
    account: Option<String>,
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
    fn from_snapshots(snapshots: &[UsageSnapshot], accounts: &[Account]) -> Self {
        let account_by_id = accounts
            .iter()
            .map(|account| (account.id.as_str().to_string(), account))
            .collect::<HashMap<_, _>>();
        let daily_tokens = aggregate_daily_tokens(snapshots);
        let overview = Overview {
            lifetime_tokens: lifetime_tokens(snapshots),
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
}

impl ProviderPanel {
    fn from_snapshot(snapshot: &UsageSnapshot, account: Option<&Account>) -> Self {
        let session_window = select_window(snapshot, WindowRole::Session);
        let weekly_window = select_window(snapshot, WindowRole::Weekly);
        let pace_window = weekly_window.or(session_window);
        let pace = pace_window.and_then(pace_line);
        let forecast = pace.as_ref().map(|pace| {
            if pace.percent_used <= pace.percent_expected + 5.0 {
                "lasts      until reset".to_string()
            } else if pace.percent_used >= 100.0 {
                "exhausted   until reset".to_string()
            } else {
                "tight       before reset".to_string()
            }
        });

        Self {
            title: provider_title(snapshot),
            session: session_window.map(|window| window_line("Session", window)),
            weekly: weekly_window.map(|window| window_line("Weekly", window)),
            pace,
            forecast,
            credits: credits_line(snapshot),
            account: account_label(snapshot, account),
        }
    }
}

fn render_dashboard(dashboard: &Dashboard, theme: Theme) -> String {
    let mut output = String::new();
    push_box(
        &mut output,
        "Overview",
        &overview_lines(&dashboard.overview, theme),
        theme,
    );
    output.push('\n');
    push_box(
        &mut output,
        "Activity · last 7 days",
        &activity_lines(&dashboard.activity, theme),
        theme,
    );
    for provider in &dashboard.providers {
        output.push('\n');
        push_box(
            &mut output,
            &provider.title,
            &provider_lines(provider, theme),
            theme,
        );
    }
    output.trim_end().to_string()
}

fn render_compact(dashboard: &Dashboard, theme: Theme) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "Tokens {} total · {} peak · streak {}d current / {}d longest",
        theme.value(&format_tokens(dashboard.overview.lifetime_tokens)),
        theme.value(&format_tokens(dashboard.overview.peak_tokens)),
        dashboard.overview.current_streak_days,
        dashboard.overview.longest_streak_days
    );

    let activity = dashboard
        .activity
        .iter()
        .map(|day| {
            format!(
                "{} {}",
                theme.muted(&day.date.format("%a").to_string()),
                theme.value(&format_tokens(day.tokens))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(output, "{} {activity}", theme.label("Activity"));

    for provider in &dashboard.providers {
        let mut parts = Vec::new();
        if let Some(session) = &provider.session {
            parts.push(compact_window(session));
        }
        if let Some(weekly) = &provider.weekly {
            parts.push(compact_window(weekly));
        }
        if let Some(credits) = &provider.credits {
            parts.push(format!("credits {}", collapse_spaces(credits)));
        }
        let account = provider
            .account
            .as_ref()
            .map(|account| format!(" · {account}"))
            .unwrap_or_default();
        let _ = writeln!(
            output,
            "{}: {}{}",
            theme.title(&provider.title),
            parts.join(", "),
            account
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
            pad_right(theme.label("Longest task"), 16),
            pad_right(theme.muted("n/a"), 10),
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

fn activity_lines(activity: &[ActivityDay], theme: Theme) -> Vec<String> {
    let peak = activity
        .iter()
        .map(|day| day.tokens)
        .max()
        .unwrap_or_default();
    activity
        .iter()
        .map(|day| {
            format!(
                "{} {}  {}",
                pad_right(theme.label(&day.date.format("%a").to_string()), 3),
                pad_left(theme.value(&format_tokens(day.tokens)), 7),
                token_bar(day.tokens, peak, BAR_WIDTH, theme)
            )
        })
        .collect()
}

fn provider_lines(provider: &ProviderPanel, theme: Theme) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(session) = &provider.session {
        lines.push(window_row(session, theme));
    }
    if let Some(weekly) = &provider.weekly {
        lines.push(window_row(weekly, theme));
    }
    if let Some(pace) = &provider.pace {
        lines.push(format!(
            "{} {} {} {} {} {}",
            pad_right(theme.label("Pace"), 8),
            pad_right(theme.pace(pace.status), 10),
            pad_left(theme.value(&format!("{:.0}%", pace.percent_used)), 4),
            theme.muted("used vs"),
            pad_left(theme.value(&format!("{:.0}%", pace.percent_expected)), 4),
            theme.muted("expected")
        ));
    }
    if let Some(forecast) = &provider.forecast {
        lines.push(format!(
            "{} {}",
            pad_right(theme.label("Forecast"), 8),
            forecast_text(forecast, theme)
        ));
    }
    if let Some(credits) = &provider.credits {
        lines.push(format!(
            "{} {}",
            pad_right(theme.label("Credits"), 8),
            credits_text(credits, theme)
        ));
    }
    if let Some(account) = &provider.account {
        lines.push(format!(
            "{} {}",
            pad_right(theme.label("Account"), 8),
            theme.muted(account)
        ));
    }
    lines
}

fn push_box(output: &mut String, title: &str, lines: &[String], theme: Theme) {
    push_top_border(output, title, theme);
    for line in lines {
        push_content_line(output, line, theme);
    }
    push_bottom_border(output, theme);
}

fn push_top_border(output: &mut String, title: &str, theme: Theme) {
    let title = truncate(title, DASHBOARD_WIDTH.saturating_sub(5));
    let fill = DASHBOARD_WIDTH.saturating_sub(visible_len(&title) + 5);
    output.push_str(&theme.border("┌─ "));
    output.push_str(&theme.title(&title));
    output.push_str(&theme.border(" "));
    output.push_str(&theme.border(&"─".repeat(fill)));
    output.push_str(&theme.border("┐"));
    output.push('\n');
}

fn push_content_line(output: &mut String, content: &str, theme: Theme) {
    let inner_width = DASHBOARD_WIDTH - 4;
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

fn push_bottom_border(output: &mut String, theme: Theme) {
    output.push_str(&theme.border("└"));
    output.push_str(&theme.border(&"─".repeat(DASHBOARD_WIDTH - 2)));
    output.push_str(&theme.border("┘"));
    output.push('\n');
}

fn compact_window(window: &WindowLine) -> String {
    let remaining = window
        .percent_remaining
        .map(format_percent)
        .unwrap_or_else(|| "?".to_string());
    format!("{} {remaining} left", window.label.to_ascii_lowercase())
}

fn window_row(line: &WindowLine, theme: Theme) -> String {
    let percent = line
        .percent_remaining
        .map(format_percent)
        .unwrap_or_else(|| "?".to_string());
    let percent = line
        .percent_remaining
        .map(|value| theme.quota(value, &percent))
        .unwrap_or_else(|| theme.muted(&percent));
    let bar = line
        .percent_remaining
        .map(|value| percent_bar(value, BAR_WIDTH, theme))
        .unwrap_or_else(|| theme.muted(&"░".repeat(BAR_WIDTH)));
    let reset = line
        .reset_at
        .map(reset_label)
        .unwrap_or_else(|| "reset unknown".to_string());
    format!(
        "{} {} {}  {}  {}",
        pad_right(theme.label(line.label), 8),
        pad_left(percent, 4),
        theme.muted("left"),
        bar,
        theme.muted(&reset)
    )
}

fn window_line(label: &'static str, window: &UsageWindow) -> WindowLine {
    WindowLine {
        label,
        percent_remaining: percent_remaining(window),
        reset_at: window.reset_at,
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

fn pace_line(window: &UsageWindow) -> Option<PaceLine> {
    let percent_used = window.percent_used?;
    let percent_expected = expected_percent_used(window)?;
    let delta = percent_used - percent_expected;
    let status = if delta > 5.0 {
        "over"
    } else if delta < -5.0 {
        "under"
    } else {
        "on track"
    };
    Some(PaceLine {
        status,
        percent_used,
        percent_expected,
    })
}

fn expected_percent_used(window: &UsageWindow) -> Option<f64> {
    let reset_at = window.reset_at?;
    let duration = expected_window_duration(window)?;
    let now = Utc::now();
    let start_at = reset_at - duration;
    let elapsed = (now - start_at).num_seconds().max(0) as f64;
    let total = duration.num_seconds().max(1) as f64;
    Some((elapsed / total * 100.0).clamp(0.0, 100.0))
}

fn expected_window_duration(window: &UsageWindow) -> Option<TimeDelta> {
    let name = format!(
        "{} {}",
        window.window_id.to_ascii_lowercase(),
        window.label.to_ascii_lowercase()
    );
    if name.contains("five_hour") || name.contains("five hour") {
        Some(TimeDelta::hours(5))
    } else if name.contains("session") {
        Some(TimeDelta::hours(5))
    } else if name.contains("seven_day") || name.contains("seven day") || name.contains("weekly") {
        Some(TimeDelta::days(7))
    } else if name.contains("daily") || name.contains("today") {
        Some(TimeDelta::days(1))
    } else if name.contains("monthly") || name.contains("30d") || name.contains("30 days") {
        Some(TimeDelta::days(30))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum WindowRole {
    Session,
    Weekly,
}

fn select_window(snapshot: &UsageSnapshot, role: WindowRole) -> Option<&UsageWindow> {
    snapshot
        .windows
        .iter()
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
    }
}

fn provider_title(snapshot: &UsageSnapshot) -> String {
    let provider = match snapshot.provider_id.as_str() {
        "codex" => "Codex".to_string(),
        "claude" => "Claude".to_string(),
        value => title_case(value),
    };
    let mut parts = vec![provider];
    if let Some(mode) = metadata_str(&snapshot.metadata, "collection_mode") {
        parts.push(collection_mode_label(snapshot.provider_id.as_str(), mode));
    }
    if let Some(plan) = metadata_str(&snapshot.metadata, "plan_type")
        .or_else(|| metadata_str(&snapshot.metadata, "subscription_type"))
    {
        parts.push(plan_label(plan));
    }
    parts.join(" · ")
}

fn collection_mode_label(provider_id: &str, mode: &str) -> String {
    match (provider_id, mode) {
        ("codex", "wham_usage_api") => "openai-web".to_string(),
        ("claude", "claude_cli_usage") => "terminal".to_string(),
        ("claude", "oauth_usage_api") => "web".to_string(),
        _ => mode.replace('_', "-"),
    }
}

fn plan_label(plan: &str) -> String {
    match plan {
        "prolite" => "Pro Lite".to_string(),
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        "team" => "Team".to_string(),
        "max" => "Max".to_string(),
        value => title_case(&value.replace(['_', '-'], " ")),
    }
}

fn account_label(snapshot: &UsageSnapshot, account: Option<&Account>) -> Option<String> {
    metadata_str(&snapshot.metadata, "email")
        .or_else(|| metadata_str(&snapshot.metadata, "account_email"))
        .or_else(|| metadata_str(&snapshot.metadata, "keychain_account"))
        .or_else(|| metadata_str(&snapshot.metadata, "account_display_name"))
        .map(str::to_string)
        .or_else(|| {
            account
                .and_then(|account| account.display_name.clone())
                .or_else(|| {
                    account
                        .map(|account| account.external_account_id.clone())
                        .filter(|value| !looks_like_uuid(value))
                })
        })
        .filter(|value| !is_provider_placeholder(snapshot, value))
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
        .filter_map(cost_metadata)
        .filter_map(|cost| {
            u64_field(cost, "total_tokens").or_else(|| u64_field(cost, "lookback_tokens"))
        })
        .sum()
}

fn daily_rows(snapshot: &UsageSnapshot) -> Vec<(NaiveDate, u64)> {
    let Some(cost) = cost_metadata(snapshot) else {
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

fn forecast_text(forecast: &str, theme: Theme) -> String {
    if forecast.contains("exhausted") {
        theme.danger(forecast)
    } else if forecast.contains("tight") {
        theme.warn(forecast)
    } else {
        theme.good(forecast)
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

fn metadata_str<'a>(metadata: &'a Value, key: &str) -> Option<&'a str> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
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

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value
            .chars()
            .all(|char| char.is_ascii_hexdigit() || char == '-')
}

fn is_provider_placeholder(snapshot: &UsageSnapshot, value: &str) -> bool {
    value.eq_ignore_ascii_case(snapshot.provider_id.as_str())
}

fn collapse_spaces(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(value: &str, max_chars: usize) -> String {
    if visible_len(value) <= max_chars {
        return value.to_string();
    }
    strip_ansi(value)
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn visible_len(value: &str) -> usize {
    let mut len = 0;
    let mut chars = value.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for char in chars.by_ref() {
                if ('@'..='~').contains(&char) {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

fn strip_ansi(value: &str) -> String {
    let mut stripped = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for char in chars.by_ref() {
                if ('@'..='~').contains(&char) {
                    break;
                }
            }
        } else {
            stripped.push(char);
        }
    }
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use usage_core::{AccountId, ProviderId};

    #[test]
    fn renders_dashboard_sections() {
        let (snapshot, account) = sample_dashboard();

        let rendered = render_usage(&[snapshot], &[account], OutputStyle::Dashboard, false);

        assert!(rendered.contains("Overview"));
        assert!(rendered.contains("Activity · last 7 days"));
        assert!(rendered.contains("Codex · openai-web · Pro Lite"));
        assert!(rendered.contains("Account  user@example.com"));
        assert!(!rendered.contains("\x1b["));
    }

    #[test]
    fn colored_dashboard_keeps_box_widths() {
        let (mut snapshot, account) = sample_dashboard();
        snapshot.windows[0].percent_used = Some(92.0);
        snapshot.windows[0].percent_remaining = Some(8.0);

        let rendered = render_usage(&[snapshot], &[account], OutputStyle::Dashboard, true);

        assert!(rendered.contains("\x1b["));
        assert!(strip_ansi(&rendered).contains("Codex · openai-web · Pro Lite"));
        for line in rendered.lines().filter(|line| !line.is_empty()) {
            assert_eq!(visible_len(line), DASHBOARD_WIDTH);
        }
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
            ],
            metadata: json!({
                "collection_mode": "wham_usage_api",
                "plan_type": "prolite",
                "email": "user@example.com",
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
            display_name: Some("Codex".to_string()),
            created_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        };

        (snapshot, account)
    }
}
