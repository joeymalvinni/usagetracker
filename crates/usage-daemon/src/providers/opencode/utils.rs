//! Small parsing, calendar, and SQLite helpers shared across OpenCode modules.

use std::{collections::BTreeSet, sync::LazyLock};

use chrono::{DateTime, Datelike, TimeDelta, TimeZone, Timelike, Utc};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::providers::{ProviderError, ProviderErrorKind};

use super::{MAX_PERCENT, OPENCODE_GO_PROVIDER_ID};

static WORKSPACE_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"wrk_[A-Za-z0-9_-]+"#).expect("valid workspace id regex"));

pub(super) fn workspace_ids_from_text(text: &str) -> Vec<String> {
    WORKSPACE_ID_REGEX
        .find_iter(text)
        .map(|match_| match_.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(super) fn regex_number(text: &str, regex: &Regex) -> Option<f64> {
    let captures = regex.captures(text)?;
    captures
        .iter()
        .flatten()
        .last()
        .and_then(|value| value.as_str().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

pub(super) fn number_from_json_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(value) => value.parse().ok().filter(|value: &f64| value.is_finite()),
        _ => None,
    }
}

pub(super) fn datetime_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(value) => DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.with_timezone(&Utc)),
        Value::Number(_) => {
            let number = number_from_json_value(value)?;
            if number > 1_000_000_000_000.0 {
                DateTime::from_timestamp_millis(number.round() as i64)
            } else {
                DateTime::from_timestamp(number.round() as i64, 0)
            }
        }
        _ => None,
    }
}

pub(super) fn normalize_percent(value: f64) -> f64 {
    let percent = if (0.0..=1.0).contains(&value) {
        value * 100.0
    } else {
        value
    };
    percent.clamp(0.0, MAX_PERCENT)
}

pub(super) fn table_exists(conn: &Connection, table: &str) -> Result<bool, ProviderError> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |_| Ok(()),
    )
    .optional()
    .map(|value| value.is_some())
    .map_err(local_db_error)
}

pub(super) fn local_db_error(err: rusqlite::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderUnavailable,
        format!("OpenCode Go local database query failed: {err}"),
    )
}

pub(super) fn utc_week_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let days = now.weekday().num_days_from_monday() as i64;
    let date = now.date_naive() - TimeDelta::days(days);
    Utc.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .unwrap_or(now)
}

pub(super) fn monthly_window_start(anchor: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
    let candidate = anchor_in_month(anchor, now.year(), now.month());
    if candidate <= now {
        candidate
    } else {
        let (year, month) = previous_month(now.year(), now.month());
        anchor_in_month(anchor, year, month)
    }
}

pub(super) fn next_monthly_anchor(anchor: DateTime<Utc>, start: DateTime<Utc>) -> DateTime<Utc> {
    let (year, month) = next_month(start.year(), start.month());
    anchor_in_month(anchor, year, month)
}

fn anchor_in_month(anchor: DateTime<Utc>, year: i32, month: u32) -> DateTime<Utc> {
    let day = anchor.day().min(days_in_month(year, month));
    Utc.with_ymd_and_hms(
        year,
        month,
        day,
        anchor.hour(),
        anchor.minute(),
        anchor.second(),
    )
    .single()
    .unwrap_or(anchor)
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = next_month(year, month);
    let next_first = Utc
        .with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0)
        .single()
        .unwrap();
    (next_first - TimeDelta::days(1)).day()
}

pub(super) fn provider_display_name(provider_id: &str) -> &'static str {
    match provider_id {
        OPENCODE_GO_PROVIDER_ID => "OpenCode Go",
        _ => "OpenCode",
    }
}

pub(super) fn provider_cookie_env() -> &'static str {
    "USAGE_TRACKER_OPENCODE_GO_COOKIE"
}

pub(super) fn provider_workspace_env() -> &'static str {
    "USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID"
}

pub(super) fn url_encode_json_arg(workspace_id: &str) -> String {
    format!("%5B%22{}%22%5D", workspace_id.replace('"', ""))
}
