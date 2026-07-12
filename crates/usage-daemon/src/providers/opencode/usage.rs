//! Web console usage, balance, and account identity parsing.

use std::sync::LazyLock;

use chrono::{DateTime, TimeDelta, Utc};
use regex::Regex;
use serde_json::{json, Map, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{ProviderError, ProviderErrorKind, ProviderUsage};

use super::{
    history::{usage_history_windows, UsageHistoryReport},
    utils::{
        datetime_from_json_value, normalize_percent, number_from_json_value, provider_display_name,
        regex_number,
    },
    MAX_PERCENT,
};

static USAGE_WINDOW_PERCENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(usagePercent|usedPercent|percentUsed|percent|usage_percent|used_percent|utilizationPercent|utilization|usage)\s*["':=]*\s*([0-9]+(?:\.[0-9]+)?)"#)
        .expect("valid usage window percent regex")
});
static USAGE_WINDOW_RESET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(resetInSec|resetInSeconds|resetSeconds|reset_sec|reset_in_sec|resetsInSec|resetsInSeconds|resetIn|resetSec)\s*["':=]*\s*([0-9]+(?:\.[0-9]+)?)"#)
        .expect("valid usage window reset regex")
});
static ROLLING_USAGE_LABEL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)Rolling\s+Usage"#).expect("valid rolling usage label regex")
});
static WEEKLY_USAGE_LABEL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)Weekly\s+Usage"#).expect("valid weekly usage label regex"));
static MONTHLY_USAGE_LABEL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)Monthly\s+Usage"#).expect("valid monthly usage label regex")
});
static USAGE_CARD_PERCENT_REGEXES: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        Regex::new(r#"(?is)data-slot=["']usage-value["'][^>]*>.*?([0-9]+(?:\.[0-9]+)?)\s*(?:<!--/-->)?\s*%"#).expect("valid usage value regex"),
        Regex::new(r#"(?is)style=["'][^"']*width\s*:\s*([0-9]+(?:\.[0-9]+)?)%"#).expect("valid usage width regex"),
    ]
});
static VISIBLE_PERCENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)([0-9]+(?:\.[0-9]+)?)\s*%"#).expect("valid visible percent regex")
});
static RESET_TIME_HTML_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)data-slot=["']reset-time["'][^>]*>(.*?)</span>"#)
        .expect("valid reset time html regex")
});
static HUMAN_DURATION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)([0-9]+(?:\.[0-9]+)?)\s*(days?|d|hours?|h|minutes?|mins?|min|m)\b"#)
        .expect("valid human duration regex")
});
static HTML_COMMENT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<!--.*?-->"#).expect("valid HTML comment regex"));
static HTML_TAG_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<[^>]+>"#).expect("valid HTML tag regex"));
static BILLING_BALANCE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""?balance"?\s*[:=]\s*(?:\$R\[\d+\]=)?(-?[0-9]+(?:\.[0-9]+)?)"#)
        .expect("valid billing balance regex")
});
static ZEN_BALANCE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)(current\s*balance|zen\s*balance|balance|現在の残高|残高).{0,120}?\$?\s*([0-9]+(?:\.[0-9]{1,2})?)"#,
    )
    .expect("valid Zen balance regex")
});
static EMAIL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}"#).expect("valid email regex")
});

#[derive(Clone, Debug)]
pub(super) struct ParsedUsage {
    pub(super) rolling: ParsedWindow,
    pub(super) weekly: Option<ParsedWindow>,
    pub(super) monthly: Option<ParsedWindow>,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedWindow {
    pub(super) percent_used: f64,
    pub(super) reset_at: Option<DateTime<Utc>>,
}

pub(super) struct UsageContext<'a> {
    pub(super) provider_id: &'static str,
    pub(super) collection_mode: &'a str,
    pub(super) zen_balance_usd: Option<f64>,
    pub(super) workspace_id: Option<&'a str>,
    pub(super) account_email: Option<&'a str>,
    pub(super) cookie_source: &'static str,
    pub(super) history: Option<UsageHistoryReport>,
}

impl ParsedUsage {
    pub(super) fn to_provider_usage(&self, context: UsageContext<'_>) -> ProviderUsage {
        let UsageContext {
            provider_id,
            collection_mode,
            zen_balance_usd,
            workspace_id,
            account_email,
            cookie_source,
            history,
        } = context;
        let mut windows = vec![usage_percent_window(
            &format!("{provider_id}_session"),
            &format!("{} session", provider_display_name(provider_id)),
            UsageWindowKind::Session,
            &self.rolling,
        )];
        if let Some(weekly) = &self.weekly {
            windows.push(usage_percent_window(
                &format!("{provider_id}_weekly"),
                &format!("{} weekly", provider_display_name(provider_id)),
                UsageWindowKind::Weekly,
                weekly,
            ));
        }
        if let Some(monthly) = &self.monthly {
            windows.push(usage_percent_window(
                &format!("{provider_id}_monthly"),
                &format!("{} monthly", provider_display_name(provider_id)),
                UsageWindowKind::Monthly,
                monthly,
            ));
        }
        if let Some(balance) = zen_balance_usd {
            windows.push(zen_balance_window(balance));
        }
        if let Some(report) = &history {
            windows.extend(usage_history_windows(provider_id, report, Utc::now()));
        }

        let mut metadata = json!({
            "collection_mode": collection_mode,
            "workspace_id": workspace_id,
            "email": account_email,
            "account_email": account_email,
            "zen_balance_usd": zen_balance_usd,
            "web_authoritative": true,
            "cookie_source": cookie_source,
        });
        if let Some(usage_history) = history.as_ref() {
            if let Some(object) = metadata.as_object_mut() {
                object.insert(
                    format!("{provider_id}_cost"),
                    usage_history.metadata_value(),
                );
            }
        }

        ProviderUsage {
            provider_id: ProviderId::new(provider_id),
            collected_at: Utc::now(),
            windows,
            metadata,
        }
    }
}

pub(super) fn parse_usage_text(
    text: &str,
    include_monthly: bool,
) -> Result<ParsedUsage, ProviderError> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(parsed) = parse_usage_json(&value, include_monthly) {
            return Ok(parsed);
        }
    }

    if let Some(parsed) = parse_usage_regex(text, include_monthly) {
        return Ok(parsed);
    }

    Err(ProviderError::new(
        ProviderErrorKind::Parse,
        "OpenCode usage response did not contain recognizable usage windows",
    ))
}

fn parse_usage_json(value: &Value, include_monthly: bool) -> Option<ParsedUsage> {
    let rolling = find_usage_window_json(
        value,
        &["rollingUsage", "rolling", "rolling_usage", "sessionUsage"],
    )?;
    let weekly = find_usage_window_json(
        value,
        &["weeklyUsage", "weekly", "weekly_usage", "weeklyWindow"],
    );
    let monthly = include_monthly
        .then(|| {
            find_usage_window_json(
                value,
                &["monthlyUsage", "monthly", "monthly_usage", "monthlyWindow"],
            )
        })
        .flatten();
    Some(ParsedUsage {
        rolling,
        weekly,
        monthly,
    })
}

fn find_usage_window_json(value: &Value, names: &[&str]) -> Option<ParsedWindow> {
    match value {
        Value::Object(object) => {
            for name in names {
                if let Some(window) = object.get(*name).and_then(parse_usage_window_object) {
                    return Some(window);
                }
            }
            if let Some(window) = parse_usage_window_object(value) {
                return Some(window);
            }
            object
                .values()
                .find_map(|child| find_usage_window_json(child, names))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_usage_window_json(child, names)),
        _ => None,
    }
}

fn parse_usage_window_object(value: &Value) -> Option<ParsedWindow> {
    let object = value.as_object()?;
    let percent = usage_percent_from_object(object)?;
    Some(ParsedWindow {
        percent_used: percent,
        reset_at: reset_at_from_object(object),
    })
}

fn usage_percent_from_object(object: &Map<String, Value>) -> Option<f64> {
    let keys = [
        "usagePercent",
        "usedPercent",
        "percentUsed",
        "percent",
        "usage_percent",
        "used_percent",
        "utilization",
        "utilizationPercent",
        "usage",
    ];
    for key in keys {
        if let Some(value) = object.get(key).and_then(number_from_json_value) {
            return Some(normalize_percent(value));
        }
    }

    let used = ["used", "consumed", "count", "usedTokens", "cost"]
        .iter()
        .find_map(|key| object.get(*key).and_then(number_from_json_value))?;
    let limit = ["limit", "total", "quota", "max", "cap", "tokenLimit"]
        .iter()
        .find_map(|key| object.get(*key).and_then(number_from_json_value))?;
    (limit > 0.0).then(|| normalize_percent((used / limit) * 100.0))
}

fn reset_at_from_object(object: &Map<String, Value>) -> Option<DateTime<Utc>> {
    for key in [
        "resetInSec",
        "resetInSeconds",
        "resetSeconds",
        "reset_sec",
        "reset_in_sec",
        "resetsInSec",
        "resetsInSeconds",
        "resetIn",
        "resetSec",
    ] {
        if let Some(seconds) = object.get(key).and_then(number_from_json_value) {
            return TimeDelta::try_seconds(seconds.round() as i64).map(|delta| Utc::now() + delta);
        }
    }
    for key in [
        "resetAt",
        "resetsAt",
        "reset_at",
        "resets_at",
        "nextReset",
        "next_reset",
        "renewAt",
        "renew_at",
    ] {
        if let Some(reset_at) = object.get(key).and_then(datetime_from_json_value) {
            return Some(reset_at);
        }
    }
    None
}

fn parse_usage_regex(text: &str, include_monthly: bool) -> Option<ParsedUsage> {
    if let Some(parsed) = parse_usage_card_text(text, include_monthly) {
        return Some(parsed);
    }

    let rolling = find_usage_window_text(
        text,
        &["rollingUsage", "rolling_usage", "rolling", "sessionUsage"],
    )?;
    let weekly = find_usage_window_text(
        text,
        &["weeklyUsage", "weekly_usage", "weekly", "weeklyWindow"],
    );
    let monthly = include_monthly
        .then(|| {
            find_usage_window_text(
                text,
                &["monthlyUsage", "monthly_usage", "monthly", "monthlyWindow"],
            )
        })
        .flatten();
    Some(ParsedUsage {
        rolling,
        weekly,
        monthly,
    })
}

/// Clamp a byte offset down to the nearest UTF-8 char boundary so slicing a
/// window out of a non-ASCII usage page cannot panic mid-codepoint.
fn clamp_to_char_boundary(text: &str, end: usize) -> usize {
    if end >= text.len() {
        return text.len();
    }
    let mut end = end;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn find_usage_window_text(text: &str, names: &[&str]) -> Option<ParsedWindow> {
    for name in names {
        let Some(index) = text.find(name) else {
            continue;
        };
        let end = clamp_to_char_boundary(text, index + 1500);
        let segment = &text[index..end];
        let percent = regex_number(segment, &USAGE_WINDOW_PERCENT_REGEX).map(normalize_percent)?;
        let reset_at = regex_number(segment, &USAGE_WINDOW_RESET_REGEX)
            .and_then(|seconds| TimeDelta::try_seconds(seconds.round() as i64))
            .map(|delta| Utc::now() + delta);
        return Some(ParsedWindow {
            percent_used: percent,
            reset_at,
        });
    }
    None
}

fn parse_usage_card_text(text: &str, include_monthly: bool) -> Option<ParsedUsage> {
    let rolling = find_labeled_usage_card(text, &["Rolling Usage"])?;
    let weekly = find_labeled_usage_card(text, &["Weekly Usage"]);
    let monthly = include_monthly
        .then(|| find_labeled_usage_card(text, &["Monthly Usage"]))
        .flatten();
    Some(ParsedUsage {
        rolling,
        weekly,
        monthly,
    })
}

fn find_labeled_usage_card(text: &str, labels: &[&str]) -> Option<ParsedWindow> {
    for label in labels {
        let regex = match *label {
            "Rolling Usage" => &*ROLLING_USAGE_LABEL_REGEX,
            "Weekly Usage" => &*WEEKLY_USAGE_LABEL_REGEX,
            "Monthly Usage" => &*MONTHLY_USAGE_LABEL_REGEX,
            _ => continue,
        };
        let Some(match_) = regex.find(text) else {
            continue;
        };
        let end = clamp_to_char_boundary(text, match_.end() + 1500);
        let segment = &text[match_.end()..end];
        let percent = usage_card_percent(segment)?;
        return Some(ParsedWindow {
            percent_used: percent,
            reset_at: usage_card_reset_at(segment),
        });
    }
    None
}

fn usage_card_percent(segment: &str) -> Option<f64> {
    // Progress-card markup can contain CSS percentages (for example, a 100%-wide
    // track) before the percentage shown to the user. Prefer percentages from
    // rendered text so those layout values cannot be mistaken for usage.
    if let Some(value) = regex_number(&html_text(segment), &VISIBLE_PERCENT_REGEX) {
        // Unlike JSON utilization fields, a value followed by `%` is already
        // expressed on a 0..100 scale. In particular, `1%` must stay 1 rather
        // than being interpreted as the fractional ratio 1.0 (100%).
        return Some(value.clamp(0.0, MAX_PERCENT));
    }

    // Keep the structural selectors as fallbacks for payloads where the value
    // is present only in an attribute or serialized component state.
    for regex in USAGE_CARD_PERCENT_REGEXES.iter() {
        if let Some(value) = regex_number(segment, regex).map(|value| value.clamp(0.0, MAX_PERCENT))
        {
            return Some(value);
        }
    }
    None
}

fn usage_card_reset_at(segment: &str) -> Option<DateTime<Utc>> {
    let reset_html = RESET_TIME_HTML_REGEX
        .captures(segment)
        .and_then(|captures| captures.get(1).map(|match_| match_.as_str().to_string()))
        .unwrap_or_else(|| segment.to_string());
    reset_at_from_human_text(&html_text(&reset_html))
}

fn reset_at_from_human_text(text: &str) -> Option<DateTime<Utc>> {
    let mut seconds = 0_i64;
    for captures in HUMAN_DURATION_REGEX.captures_iter(text) {
        let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
        let unit = captures.get(2)?.as_str().to_ascii_lowercase();
        seconds += match unit.as_str() {
            "day" | "days" | "d" => (value * 86_400.0).round() as i64,
            "hour" | "hours" | "h" => (value * 3_600.0).round() as i64,
            "minute" | "minutes" | "min" | "mins" | "m" => (value * 60.0).round() as i64,
            _ => 0,
        };
    }
    (seconds > 0)
        .then(|| TimeDelta::try_seconds(seconds).map(|delta| Utc::now() + delta))
        .flatten()
}

fn html_text(text: &str) -> String {
    let without_comments = HTML_COMMENT_REGEX.replace_all(text, " ").into_owned();
    let without_tags = HTML_TAG_REGEX
        .replace_all(&without_comments, " ")
        .into_owned();
    without_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn parse_zen_balance(text: &str) -> Option<f64> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(balance) = find_zen_balance_json(&value) {
            return Some(balance);
        }
    }
    find_billing_balance_text(text).or_else(|| find_zen_balance_text(text))
}

fn find_zen_balance_json(value: &Value) -> Option<f64> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let normalized = key
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if matches!(
                    normalized.as_str(),
                    "zenbalance"
                        | "zencurrentbalance"
                        | "currentbalance"
                        | "currentbalanceusd"
                        | "balanceusd"
                        | "usdbalance"
                ) {
                    if let Some(balance) = number_from_json_value(value) {
                        return Some(balance);
                    }
                }
            }
            object.values().find_map(find_zen_balance_json)
        }
        Value::Array(values) => values.iter().find_map(find_zen_balance_json),
        _ => None,
    }
}

fn find_billing_balance_text(text: &str) -> Option<f64> {
    if !text.contains("customerID") && !text.contains("customerId") {
        return None;
    }
    let value = regex_number(text, &BILLING_BALANCE_REGEX)?;
    Some(value / 100_000_000.0)
}

fn find_zen_balance_text(text: &str) -> Option<f64> {
    ZEN_BALANCE_REGEX
        .captures(text)
        .and_then(|captures| captures.get(2))
        .and_then(|value| value.as_str().parse::<f64>().ok())
}

pub(super) fn account_email_from_text(text: &str) -> Option<String> {
    EMAIL_REGEX
        .find(text)
        .map(|match_| match_.as_str().to_string())
}

fn usage_percent_window(
    window_id: &str,
    label: &str,
    kind: UsageWindowKind,
    parsed: &ParsedWindow,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used: Some(parsed.percent_used),
        percent_remaining: Some(MAX_PERCENT - parsed.percent_used),
        reset_at: parsed.reset_at,
    }
}

fn zen_balance_window(balance: f64) -> UsageWindow {
    UsageWindow {
        window_id: "opencode_go_zen_balance".to_string(),
        label: "OpenCode Go Zen balance".to_string(),
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: Some(UsageAmount {
            value: balance,
            unit: UsageUnit::Usd,
        }),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

#[cfg(test)]
mod usage_slice_tests {
    use super::*;

    #[test]
    fn window_scan_does_not_panic_on_multibyte_bodies() {
        // A usage page whose match is followed by non-ASCII content that would
        // straddle the byte offset `index + 1500` used to be sliced blindly and
        // panic on a UTF-8 char boundary. Pad with multibyte codepoints so the
        // window is well beyond 1500 bytes past the match.
        let padding = "現".repeat(2000);
        let body = format!("rollingUsage {padding} usagePercent: 42");

        let parsed = find_usage_window_text(&body, &["rollingUsage"]);

        // The percent lives past the slice window, so it is simply not found —
        // the point is that the scan returns instead of panicking.
        assert!(parsed.is_none());
    }

    #[test]
    fn labeled_card_scan_does_not_panic_on_multibyte_bodies() {
        let padding = "残".repeat(2000);
        let body = format!("Rolling Usage {padding}");

        let parsed = find_labeled_usage_card(&body, &["Rolling Usage"]);

        assert!(parsed.is_none());
    }

    #[test]
    fn clamp_never_splits_a_codepoint() {
        let text = "aaa現現現";
        for end in 0..=text.len() + 5 {
            let clamped = clamp_to_char_boundary(text, end);
            assert!(text.is_char_boundary(clamped));
            assert!(clamped <= text.len());
        }
    }
}
