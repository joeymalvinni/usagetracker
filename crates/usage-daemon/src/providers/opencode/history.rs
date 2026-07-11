//! Usage history parsing, aggregation, and cost/token window generation.

use std::{collections::BTreeMap, sync::LazyLock};

use chrono::{DateTime, Local, NaiveDate, Utc};
use regex::Regex;
use serde_json::{json, Value};
use usage_core::{UsageWindow, UsageWindowKind};

use crate::providers::local_usage::{
    cost_window, daily_cost_rows, lookback_start, token_window, DailyCostSummary, DailyRollup,
};

use super::{local::LocalUsageRow, utils::provider_display_name, COST_LOOKBACK_DAYS};

static USAGE_HISTORY_ROW_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)timeCreated:\s*(?:\$R\[\d+\]\s*=\s*)?new Date\(["']([^"']+)["']\).*?inputTokens:\s*(null|[0-9]+).*?outputTokens:\s*(null|[0-9]+).*?reasoningTokens:\s*(null|[0-9]+).*?cacheReadTokens:\s*(null|[0-9]+).*?cost:\s*(null|[0-9]+)"#,
    )
    .expect("valid usage history row regex")
});

#[derive(Default)]
pub(super) struct UsageHistoryCollection {
    pub(super) report: Option<UsageHistoryReport>,
    pub(super) raw_pages: Vec<String>,
    pub(super) account_email: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct UsageHistoryRow {
    pub(super) created_at: DateTime<Utc>,
    pub(super) tokens: u64,
    pub(super) cost_usd: f64,
}

#[derive(Clone, Default)]
pub(super) struct UsageHistoryReport {
    pub(super) source: &'static str,
    pub(super) estimate: bool,
    pub(super) partial: bool,
    pub(super) complete_lookback: bool,
    pub(super) row_count: u64,
    pub(super) total_tokens: u64,
    pub(super) total_cost_usd: f64,
    pub(super) latest_at: Option<DateTime<Utc>>,
    pub(super) by_day: BTreeMap<NaiveDate, DailyCostSummary>,
}

impl UsageHistoryReport {
    pub(super) fn metadata_value(&self) -> Value {
        let now = Local::now();
        let rollup =
            DailyRollup::from_days(&self.by_day, now.date_naive(), COST_LOOKBACK_DAYS as u64);
        json!({
            "source": self.source,
            "estimate": self.estimate,
            "partial": self.partial,
            "complete_lookback": self.complete_lookback,
            "row_count": self.row_count,
            "today_cost_usd": rollup.today.cost_usd,
            "today_tokens": rollup.today.tokens,
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_cost_usd": rollup.lookback.cost_usd,
            "lookback_tokens": rollup.lookback.tokens,
            "total_tokens": self.total_tokens,
            "total_cost_usd": self.total_cost_usd,
            "latest_usage_at": self.latest_at.map(|time| time.to_rfc3339()),
            "by_day": daily_cost_rows(&self.by_day),
        })
    }
}

pub(super) fn parse_usage_history_report(text: &str) -> Option<UsageHistoryReport> {
    if !text.contains("usage.list") {
        return None;
    }
    usage_history_report_from_rows(
        parse_usage_history_rows(text),
        "opencode_usage_page",
        true,
        false,
    )
}

pub(super) fn parse_usage_history_rows(text: &str) -> Vec<UsageHistoryRow> {
    USAGE_HISTORY_ROW_REGEX
        .captures_iter(text)
        .filter_map(|captures| {
            let created_at = DateTime::parse_from_rfc3339(captures.get(1)?.as_str())
                .ok()?
                .with_timezone(&Utc);
            let input = optional_u64(captures.get(2)?.as_str());
            let output = optional_u64(captures.get(3)?.as_str());
            let reasoning = optional_u64(captures.get(4)?.as_str());
            let cache_read = optional_u64(captures.get(5)?.as_str());
            let cost_usd = optional_u64(captures.get(6)?.as_str()) as f64 / 100_000_000.0;
            let tokens = input
                .saturating_add(output)
                .saturating_add(reasoning)
                .saturating_add(cache_read);
            Some(UsageHistoryRow {
                created_at,
                tokens,
                cost_usd,
            })
        })
        .collect()
}

pub(super) fn usage_history_report_from_rows(
    rows: Vec<UsageHistoryRow>,
    source: &'static str,
    partial: bool,
    complete_lookback: bool,
) -> Option<UsageHistoryReport> {
    if rows.is_empty() {
        return None;
    }
    let mut report = UsageHistoryReport {
        source,
        partial,
        complete_lookback,
        ..UsageHistoryReport::default()
    };

    for row in rows {
        let day = report
            .by_day
            .entry(row.created_at.with_timezone(&Local).date_naive())
            .or_default();
        day.tokens = day.tokens.saturating_add(row.tokens);
        day.priced_tokens = day.priced_tokens.saturating_add(row.tokens);
        day.cost_usd += row.cost_usd;
        day.rows = day.rows.saturating_add(1);
        report.total_tokens = report.total_tokens.saturating_add(row.tokens);
        report.total_cost_usd += row.cost_usd;
        report.row_count = report.row_count.saturating_add(1);
        report.latest_at = Some(
            report
                .latest_at
                .map_or(row.created_at, |current| current.max(row.created_at)),
        );
    }

    Some(report)
}

fn optional_u64(value: &str) -> u64 {
    if value.eq_ignore_ascii_case("null") {
        0
    } else {
        value.parse().unwrap_or(0)
    }
}

pub(super) fn local_usage_history_report(
    rows: &[LocalUsageRow],
    now: DateTime<Utc>,
) -> Option<UsageHistoryReport> {
    if rows.is_empty() {
        return None;
    }
    let mut report = UsageHistoryReport {
        source: "opencode_local_sqlite",
        estimate: true,
        partial: false,
        complete_lookback: true,
        ..UsageHistoryReport::default()
    };
    for row in rows {
        if row.created_at > now {
            continue;
        }
        let day = report
            .by_day
            .entry(row.created_at.with_timezone(&Local).date_naive())
            .or_default();
        day.cost_usd += row.cost;
        day.rows = day.rows.saturating_add(1);
        report.row_count = report.row_count.saturating_add(1);
        report.total_cost_usd += row.cost;
        report.latest_at = Some(
            report
                .latest_at
                .map_or(row.created_at, |current| current.max(row.created_at)),
        );
    }
    (report.row_count > 0).then_some(report)
}

pub(super) fn usage_history_windows(
    provider_id: &str,
    report: &UsageHistoryReport,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let local_now = now.with_timezone(&Local);
    let rollup = DailyRollup::from_days(
        &report.by_day,
        local_now.date_naive(),
        COST_LOOKBACK_DAYS as u64,
    );
    let today_cost = rollup.today.cost_usd;
    let today_tokens = rollup.today.tokens;
    let lookback_cost = rollup.lookback.cost_usd;
    let lookback_tokens = rollup.lookback.tokens;
    let display_name = provider_display_name(provider_id);
    let mut windows = Vec::new();
    if today_cost > 0.0 {
        windows.push(cost_window(
            format!("{provider_id}_spend_today"),
            format!("{display_name} spend today"),
            today_cost,
        ));
    }
    if today_tokens > 0 {
        windows.push(token_window(
            format!("{provider_id}_tokens_today"),
            format!("{display_name} tokens today"),
            today_tokens,
            UsageWindowKind::Daily,
        ));
    }
    if lookback_cost > 0.0 {
        windows.push(cost_window(
            format!("{provider_id}_spend_30d"),
            format!("{display_name} spend 30 days"),
            lookback_cost,
        ));
    }
    if lookback_tokens > 0 {
        windows.push(token_window(
            format!("{provider_id}_tokens_30d"),
            format!("{display_name} tokens 30 days"),
            lookback_tokens,
            UsageWindowKind::Monthly,
        ));
    }
    windows
}

pub(super) fn usage_history_lookback_start(now: DateTime<Local>) -> chrono::NaiveDate {
    lookback_start(now.date_naive(), COST_LOOKBACK_DAYS as u64)
}
