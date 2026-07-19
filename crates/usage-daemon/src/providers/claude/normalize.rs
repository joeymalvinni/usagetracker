use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{
    local_usage::{stable_window_fragment, usage_kind_from_name},
    ProviderError, ProviderErrorKind, ProviderUsage,
};

use super::{credentials::ClaudeCredentials, CLAUDE_COLLECTION_MODE, PROVIDER_ID};

const MAX_PERCENT: f64 = 100.0;
const UNIX_MILLIS_THRESHOLD: f64 = 10_000_000_000.0;

pub(super) fn normalize_usage(
    payload: &Value,
    credentials: &ClaudeCredentials,
) -> Result<ProviderUsage, ProviderError> {
    let object = payload.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Claude usage response was not a JSON object",
        )
    })?;

    let response: ClaudeUsageResponse = serde_json::from_value(payload.clone()).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Claude usage response shape was invalid: {err}"),
        )
    })?;

    // Newer usage responses expose the complete, display-ready quota list in
    // `limits`. In particular, scoped model limits (for example Fable) only
    // appear there while their legacy top-level field remains null. Keep every
    // canonical window that parses, then fill only missing stable IDs from the
    // legacy shapes so one malformed canonical entry cannot hide a valid quota.
    let mut windows = limit_windows(response.limits.as_ref());
    let mut legacy_windows = utilization_windows(response.utilization.as_ref());
    legacy_windows.extend(recursive_utilization_windows(payload));
    merge_missing_windows(&mut windows, legacy_windows);
    if let Some(extra_usage) = response.extra_usage.as_ref().and_then(extra_usage_window) {
        windows.push(extra_usage);
    }

    if windows.is_empty() {
        let top_level_keys = object.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Claude usage response did not contain usage windows; top-level keys: {top_level_keys}"),
        ));
    }

    let top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "collection_mode": CLAUDE_COLLECTION_MODE,
            "keychain_service": credentials.keychain_service,
            "keychain_account": credentials.keychain_account,
            "subscription_type": credentials.subscription_type,
            "rate_limit_tier": credentials.rate_limit_tier,
            "token_expires_at_ms": credentials.expires_at_ms,
            "scopes": credentials.scopes,
            "extra_usage_enabled": response.extra_usage.as_ref().and_then(ClaudeExtraUsage::enabled),
            "top_level_keys": top_level_keys,
        }),
    })
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    utilization: Option<BTreeMap<String, ClaudeUtilizationEntry>>,
    #[serde(default)]
    limits: Option<Value>,
    #[serde(default)]
    extra_usage: Option<ClaudeExtraUsage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeLimit {
    kind: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    percent: Option<NumberLike>,
    #[serde(default, alias = "resetsAt", alias = "reset_at", alias = "resetAt")]
    resets_at: Option<Value>,
    #[serde(default)]
    scope: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ClaudeUtilizationEntry {
    Percent(NumberLike),
    Window(ClaudeUtilizationWindow),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NumberLike {
    Number(f64),
    String(String),
}

impl NumberLike {
    fn value(&self) -> Option<f64> {
        match self {
            Self::Number(value) => value.is_finite().then_some(*value),
            Self::String(value) => value.parse().ok().filter(|value: &f64| value.is_finite()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeUtilizationWindow {
    #[serde(
        default,
        alias = "used_percent",
        alias = "usedPercent",
        alias = "percent_used",
        alias = "percentUsed"
    )]
    utilization: Option<NumberLike>,
    #[serde(default, alias = "resetsAt", alias = "reset_at", alias = "resetAt")]
    resets_at: Option<Value>,
    #[serde(default, alias = "reset_date", alias = "resetDate")]
    reset_date: Option<Value>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "rateLimitType")]
    rate_limit_type: Option<String>,
    #[serde(default)]
    claim: Option<String>,
}

impl ClaudeUtilizationWindow {
    fn percent_used(&self) -> Option<f64> {
        self.utilization.as_ref().and_then(NumberLike::value)
    }

    fn reset_at(&self) -> Option<DateTime<Utc>> {
        self.resets_at
            .as_ref()
            .or(self.reset_date.as_ref())
            .and_then(date_time_from_json_value)
    }

    fn label(&self) -> Option<String> {
        [
            self.label.as_deref(),
            self.name.as_deref(),
            self.title.as_deref(),
            self.rate_limit_type.as_deref(),
            self.claim.as_deref(),
        ]
        .into_iter()
        .flatten()
        .next()
        .map(humanize_window_label)
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeExtraUsage {
    #[serde(
        default,
        alias = "currentUsage",
        alias = "used",
        alias = "usage",
        alias = "spent",
        alias = "spent_usd"
    )]
    current_usage: Option<NumberLike>,
    #[serde(
        default,
        alias = "monthlyLimit",
        alias = "limit",
        alias = "spend_limit",
        alias = "spendLimit"
    )]
    monthly_limit: Option<NumberLike>,
    #[serde(default, alias = "enabled")]
    is_enabled: Option<bool>,
    #[serde(default, alias = "resetsAt", alias = "reset_at", alias = "resetAt")]
    resets_at: Option<Value>,
    #[serde(default, alias = "reset_date", alias = "resetDate")]
    reset_date: Option<Value>,
}

impl ClaudeExtraUsage {
    fn enabled(&self) -> Option<bool> {
        self.is_enabled
    }

    fn reset_at(&self) -> Option<DateTime<Utc>> {
        self.resets_at
            .as_ref()
            .or(self.reset_date.as_ref())
            .and_then(date_time_from_json_value)
    }
}

fn limit_windows(limits: Option<&Value>) -> Vec<UsageWindow> {
    limits
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|limit| serde_json::from_value::<ClaudeLimit>(limit.clone()).ok())
        .filter_map(|limit| {
            let percent_used = limit.percent.as_ref()?.value()?;
            let scope = limit.scope.as_ref().and_then(limit_scope_label);
            let (name, label) = match (limit.kind.as_str(), scope.as_deref()) {
                // Preserve the established IDs for the two general quotas so
                // forecasts, alerts, and hidden-window preferences remain stable.
                ("session", _) => ("five_hour".to_string(), "Claude five hour".to_string()),
                ("weekly_all", _) => ("seven_day".to_string(), "Claude seven day".to_string()),
                ("weekly_scoped", Some(scope)) => (
                    format!("seven_day_{scope}"),
                    format!("Claude current week ({scope})"),
                ),
                (kind, Some(scope)) => (
                    format!("{kind}_{scope}"),
                    format!("Claude {} ({scope})", humanize_words(kind)),
                ),
                (kind, None) => (kind.to_string(), humanize_window_label(kind)),
            };
            let kind_name = limit.group.as_deref().unwrap_or(&limit.kind);

            Some(percent_window(PercentWindowSpec {
                name,
                label: Some(label),
                percent_used,
                reset_at: limit.resets_at.as_ref().and_then(date_time_from_json_value),
                kind: Some(usage_kind_from_name(kind_name)),
            }))
        })
        .collect()
}

fn limit_scope_label(scope: &Value) -> Option<String> {
    let scope = scope.as_object()?;
    ["model", "surface"]
        .into_iter()
        .filter_map(|key| scope.get(key))
        .find_map(|value| match value {
            Value::String(value) => nonempty(value),
            Value::Object(value) => ["display_name", "name", "id"]
                .into_iter()
                .filter_map(|key| value.get(key).and_then(Value::as_str).and_then(nonempty))
                .next(),
            _ => None,
        })
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn merge_missing_windows(windows: &mut Vec<UsageWindow>, candidates: Vec<UsageWindow>) {
    for candidate in candidates {
        if windows
            .iter()
            .all(|window| window.window_id != candidate.window_id)
        {
            windows.push(candidate);
        }
    }
}

fn utilization_windows(
    utilization: Option<&BTreeMap<String, ClaudeUtilizationEntry>>,
) -> Vec<UsageWindow> {
    let Some(utilization) = utilization else {
        return Vec::new();
    };

    utilization
        .iter()
        .filter_map(|(name, entry)| match entry {
            ClaudeUtilizationEntry::Percent(percent) => percent.value().map(|value| {
                percent_window(PercentWindowSpec {
                    name: name.to_string(),
                    label: None,
                    percent_used: value,
                    reset_at: None,
                    kind: None,
                })
            }),
            ClaudeUtilizationEntry::Window(window) => window.percent_used().map(|value| {
                percent_window(PercentWindowSpec {
                    name: name.to_string(),
                    label: window.label(),
                    percent_used: value,
                    reset_at: window.reset_at(),
                    kind: None,
                })
            }),
        })
        .collect()
}

fn recursive_utilization_windows(payload: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    let mut path = Vec::new();
    collect_recursive_utilization_windows(payload, &mut path, &mut windows);
    windows
}

fn collect_recursive_utilization_windows(
    value: &Value,
    path: &mut Vec<String>,
    windows: &mut Vec<UsageWindow>,
) {
    if let Some(window) = path_window(path, value) {
        windows.push(window);
        return;
    }

    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "extra_usage" {
                    continue;
                }
                if key == "utilization" {
                    if !path.is_empty() {
                        collect_nested_utilization_windows(path, child, windows);
                    }
                    continue;
                }

                path.push(key.clone());
                collect_recursive_utilization_windows(child, path, windows);
                path.pop();
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                path.push(index.to_string());
                collect_recursive_utilization_windows(child, path, windows);
                path.pop();
            }
        }
        _ => {}
    }
}

fn collect_nested_utilization_windows(
    parent_path: &[String],
    value: &Value,
    windows: &mut Vec<UsageWindow>,
) {
    let Some(utilization) = value.as_object() else {
        return;
    };

    for (name, entry) in utilization {
        let mut path = parent_path.to_vec();
        path.push(name.clone());
        if let Ok(entry) = serde_json::from_value::<ClaudeUtilizationEntry>(entry.clone()) {
            if let Some(window) = utilization_entry_window(&path, entry) {
                windows.push(window);
            }
        }
    }
}

fn path_window(path: &[String], value: &Value) -> Option<UsageWindow> {
    if path.is_empty() {
        return None;
    }

    let entry = serde_json::from_value::<ClaudeUtilizationEntry>(value.clone()).ok()?;
    utilization_entry_window(path, entry)
}

fn utilization_entry_window(path: &[String], entry: ClaudeUtilizationEntry) -> Option<UsageWindow> {
    let name = path.join("_");
    let label = path.last().map(humanize_window_label);

    match entry {
        ClaudeUtilizationEntry::Percent(percent) => {
            if !looks_like_usage_window_name(path.last()?) {
                return None;
            }
            percent.value().map(|value| {
                percent_window(PercentWindowSpec {
                    name,
                    label,
                    percent_used: value,
                    reset_at: None,
                    kind: None,
                })
            })
        }
        ClaudeUtilizationEntry::Window(window) => window.percent_used().map(|value| {
            percent_window(PercentWindowSpec {
                name,
                label: window.label().or(label),
                percent_used: value,
                reset_at: window.reset_at(),
                kind: None,
            })
        }),
    }
}

fn looks_like_usage_window_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("hour")
        || name.contains("day")
        || name.contains("week")
        || name.contains("month")
        || name.contains("session")
}

struct PercentWindowSpec {
    name: String,
    label: Option<String>,
    percent_used: f64,
    reset_at: Option<DateTime<Utc>>,
    kind: Option<UsageWindowKind>,
}

fn percent_window(spec: PercentWindowSpec) -> UsageWindow {
    let percent_used = spec.percent_used.clamp(0.0, MAX_PERCENT);
    let percent_remaining = MAX_PERCENT - percent_used;

    UsageWindow {
        window_id: format!(
            "claude_usage_utilization_{}",
            stable_window_fragment(&spec.name)
        ),
        label: spec
            .label
            .unwrap_or_else(|| humanize_window_label(&spec.name)),
        kind: spec
            .kind
            .unwrap_or_else(|| usage_kind_from_name(&spec.name)),
        used: Some(UsageAmount {
            value: percent_used,
            unit: UsageUnit::Percent,
        }),
        limit: Some(UsageAmount {
            value: MAX_PERCENT,
            unit: UsageUnit::Percent,
        }),
        remaining: Some(UsageAmount {
            value: percent_remaining,
            unit: UsageUnit::Percent,
        }),
        percent_used: Some(percent_used),
        percent_remaining: Some(percent_remaining),
        reset_at: spec.reset_at,
    }
}

fn extra_usage_window(extra_usage: &ClaudeExtraUsage) -> Option<UsageWindow> {
    let used = extra_usage
        .current_usage
        .as_ref()
        .and_then(NumberLike::value);
    let limit = extra_usage
        .monthly_limit
        .as_ref()
        .and_then(NumberLike::value);

    if used.is_none() && limit.is_none() {
        return None;
    }

    let remaining = used.zip(limit).map(|(used, limit)| (limit - used).max(0.0));
    let percent_used = used
        .zip(limit)
        .filter(|(_, limit)| *limit > 0.0)
        .map(|(used, limit)| (used / limit * MAX_PERCENT).clamp(0.0, MAX_PERCENT));

    Some(UsageWindow {
        window_id: "claude_extra_usage".to_string(),
        label: "Claude extra usage".to_string(),
        kind: UsageWindowKind::Credits,
        used: used.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        limit: limit.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        remaining: remaining.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        percent_used,
        percent_remaining: percent_used.map(|value| MAX_PERCENT - value),
        reset_at: extra_usage.reset_at(),
    })
}

fn date_time_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(value) => DateTime::parse_from_rfc3339(value)
            .map(|value| value.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|value| value.and_utc())
            }),
        Value::Number(number) => {
            let timestamp = number.as_f64()?;
            if timestamp > UNIX_MILLIS_THRESHOLD {
                Utc.timestamp_millis_opt(timestamp.round() as i64).single()
            } else {
                Utc.timestamp_opt(timestamp.round() as i64, 0).single()
            }
        }
        _ => None,
    }
}

fn humanize_words(value: impl AsRef<str>) -> String {
    value.as_ref().replace(['_', '-'], " ")
}

fn humanize_window_label(value: impl AsRef<str>) -> String {
    let value = humanize_words(value);
    let value = value.trim();
    if value.to_ascii_lowercase().starts_with("claude") {
        value.to_string()
    } else {
        format!("Claude {value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::AccountId;

    use crate::providers::claude::credentials::{parse_credentials, CredentialSource};

    #[test]
    fn normalizes_oauth_usage_utilization() {
        let credentials = test_credentials();
        let payload = json!({
            "utilization": {
                "five_hour": {
                    "utilization": 42.5,
                    "resets_at": "2026-06-12T08:00:00Z",
                    "rate_limit_type": "five hour"
                },
                "weekly": {
                    "usedPercent": 5,
                    "resetDate": "2026-06-18T22:09:34Z"
                }
            },
            "extra_usage": {
                "is_enabled": true,
                "current_usage": 12.5,
                "monthly_limit": 100.0
            }
        });

        let snapshot = normalize_usage(&payload, &credentials)
            .unwrap()
            .into_snapshot(AccountId::new("joey"));
        assert_eq!(snapshot.provider_id, ProviderId::new("claude"));
        assert_eq!(snapshot.windows.len(), 3);

        let five_hour = find_window(&snapshot.windows, "claude_usage_utilization_five_hour");
        assert!(matches!(five_hour.kind, UsageWindowKind::Session));
        assert_eq!(five_hour.label, "Claude five hour");
        assert_eq!(five_hour.used.as_ref().unwrap().value, 42.5);
        assert!(matches!(
            five_hour.used.as_ref().unwrap().unit,
            UsageUnit::Percent
        ));
        assert_eq!(five_hour.limit.as_ref().unwrap().value, 100.0);
        assert_eq!(five_hour.remaining.as_ref().unwrap().value, 57.5);
        assert_eq!(five_hour.percent_used, Some(42.5));
        assert_eq!(five_hour.percent_remaining, Some(57.5));
        assert_eq!(five_hour.reset_at.unwrap().timestamp(), 1781251200);

        let weekly = find_window(&snapshot.windows, "claude_usage_utilization_weekly");
        assert!(matches!(weekly.kind, UsageWindowKind::Weekly));
        assert_eq!(weekly.percent_used, Some(5.0));
        assert_eq!(weekly.percent_remaining, Some(95.0));

        let extra = find_window(&snapshot.windows, "claude_extra_usage");
        assert!(matches!(extra.kind, UsageWindowKind::Credits));
        assert_eq!(extra.used.as_ref().unwrap().value, 12.5);
        assert_eq!(extra.limit.as_ref().unwrap().value, 100.0);
        assert_eq!(extra.remaining.as_ref().unwrap().value, 87.5);
        assert_eq!(extra.percent_used, Some(12.5));

        assert_eq!(snapshot.metadata["collection_mode"], CLAUDE_COLLECTION_MODE);
        assert_eq!(snapshot.metadata["subscription_type"], "team");
        assert_eq!(snapshot.metadata["extra_usage_enabled"], true);
    }

    #[test]
    fn normalizes_numeric_utilization_values() {
        let snapshot = normalize_usage(
            &json!({
                "utilization": {
                    "daily": 9.25
                }
            }),
            &test_credentials(),
        )
        .unwrap()
        .into_snapshot(AccountId::new("joey"));

        let daily = find_window(&snapshot.windows, "claude_usage_utilization_daily");
        assert!(matches!(daily.kind, UsageWindowKind::Daily));
        assert_eq!(daily.percent_used, Some(9.25));
        assert_eq!(daily.remaining.as_ref().unwrap().value, 90.75);
    }

    #[test]
    fn normalizes_canonical_limits_including_scoped_models() {
        let snapshot = normalize_usage(
            &json!({
                "five_hour": {
                    "utilization": 19,
                    "resets_at": "2026-07-16T09:00:00Z"
                },
                "seven_day": {
                    "utilization": 2,
                    "resets_at": "2026-07-22T01:00:00Z"
                },
                "seven_day_omelette": null,
                "limits": [
                    {
                        "kind": "session",
                        "group": "session",
                        "percent": 19,
                        "resets_at": "2026-07-16T09:00:00Z",
                        "scope": null,
                        "is_active": true
                    },
                    {
                        "kind": "weekly_all",
                        "group": "weekly",
                        "percent": 2,
                        "resets_at": "2026-07-22T01:00:00Z",
                        "scope": null,
                        "is_active": false
                    },
                    {
                        "kind": "weekly_scoped",
                        "group": "weekly",
                        "percent": 4,
                        "resets_at": "2026-07-22T01:00:00Z",
                        "scope": {
                            "model": {"id": null, "display_name": "Fable"},
                            "surface": null
                        },
                        "is_active": false
                    }
                ]
            }),
            &test_credentials(),
        )
        .unwrap()
        .into_snapshot(AccountId::new("joey"));

        assert_eq!(snapshot.windows.len(), 3);

        let session = find_window(&snapshot.windows, "claude_usage_utilization_five_hour");
        assert!(matches!(session.kind, UsageWindowKind::Session));
        assert_eq!(session.percent_used, Some(19.0));

        let weekly = find_window(&snapshot.windows, "claude_usage_utilization_seven_day");
        assert!(matches!(weekly.kind, UsageWindowKind::Weekly));
        assert_eq!(weekly.percent_used, Some(2.0));

        let fable = find_window(
            &snapshot.windows,
            "claude_usage_utilization_seven_day_fable",
        );
        assert!(matches!(fable.kind, UsageWindowKind::Weekly));
        assert_eq!(fable.label, "Claude current week (Fable)");
        assert_eq!(fable.percent_used, Some(4.0));
        assert_eq!(fable.percent_remaining, Some(96.0));
        assert_eq!(
            fable.reset_at.unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 22, 1, 0, 0).unwrap()
        );
    }

    #[test]
    fn scoped_limit_uses_first_nonempty_identity_field() {
        let snapshot = normalize_usage(
            &json!({
                "limits": [{
                    "kind": "weekly_scoped",
                    "group": "weekly",
                    "percent": 4,
                    "scope": {
                        "model": {
                            "display_name": null,
                            "name": "  ",
                            "id": "claude-fable"
                        }
                    }
                }]
            }),
            &test_credentials(),
        )
        .unwrap()
        .into_snapshot(AccountId::new("joey"));

        let scoped = find_window(
            &snapshot.windows,
            "claude_usage_utilization_seven_day_claude_fable",
        );
        assert_eq!(scoped.label, "Claude current week (claude-fable)");
        assert_eq!(scoped.percent_used, Some(4.0));
    }

    #[test]
    fn fills_missing_canonical_limits_from_legacy_windows() {
        let snapshot = normalize_usage(
            &json!({
                "five_hour": {
                    "utilization": 99,
                    "resets_at": "2026-07-16T09:00:00Z"
                },
                "seven_day": {
                    "utilization": 2,
                    "resets_at": "2026-07-22T01:00:00Z"
                },
                "limits": [
                    {
                        "kind": "session",
                        "group": "session",
                        "percent": 19,
                        "resets_at": "2026-07-16T09:00:00Z"
                    },
                    {
                        "kind": "weekly_all",
                        "group": "weekly",
                        "percent": null,
                        "resets_at": "2026-07-22T01:00:00Z"
                    }
                ]
            }),
            &test_credentials(),
        )
        .unwrap()
        .into_snapshot(AccountId::new("joey"));

        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            find_window(&snapshot.windows, "claude_usage_utilization_five_hour").percent_used,
            Some(19.0)
        );
        assert_eq!(
            find_window(&snapshot.windows, "claude_usage_utilization_seven_day").percent_used,
            Some(2.0)
        );
    }

    #[test]
    fn normalizes_top_level_usage_windows() {
        let snapshot = normalize_usage(
            &json!({
                "five_hour": {
                    "utilization": 42.5,
                    "resets_at": "2026-06-12T08:00:00Z",
                    "rate_limit_type": "five hour"
                },
                "seven_day_sonnet": {
                    "usedPercent": "17.5",
                    "resetDate": "2026-06-18T22:09:34Z"
                },
                "seven_day_opus": 5,
                "tangelo": 8
            }),
            &test_credentials(),
        )
        .unwrap()
        .into_snapshot(AccountId::new("joey"));

        assert_eq!(snapshot.windows.len(), 3);

        let five_hour = find_window(&snapshot.windows, "claude_usage_utilization_five_hour");
        assert!(matches!(five_hour.kind, UsageWindowKind::Session));
        assert_eq!(five_hour.label, "Claude five hour");
        assert_eq!(five_hour.percent_used, Some(42.5));

        let sonnet = find_window(
            &snapshot.windows,
            "claude_usage_utilization_seven_day_sonnet",
        );
        assert!(matches!(sonnet.kind, UsageWindowKind::Daily));
        assert_eq!(sonnet.label, "Claude seven day sonnet");
        assert_eq!(sonnet.percent_used, Some(17.5));

        let opus = find_window(&snapshot.windows, "claude_usage_utilization_seven_day_opus");
        assert_eq!(opus.percent_used, Some(5.0));
    }

    #[test]
    fn recursively_normalizes_nested_usage_windows() {
        let snapshot = normalize_usage(
            &json!({
                "limits": {
                    "utilization": {
                        "five_hour": 25
                    },
                    "weekly": {
                        "percent_used": 70
                    }
                }
            }),
            &test_credentials(),
        )
        .unwrap()
        .into_snapshot(AccountId::new("joey"));

        let five_hour = find_window(
            &snapshot.windows,
            "claude_usage_utilization_limits_five_hour",
        );
        assert_eq!(five_hour.label, "Claude five hour");
        assert_eq!(five_hour.percent_used, Some(25.0));

        let weekly = find_window(&snapshot.windows, "claude_usage_utilization_limits_weekly");
        assert_eq!(weekly.label, "Claude weekly");
        assert_eq!(weekly.percent_used, Some(70.0));
    }

    #[test]
    fn rejects_usage_without_windows() {
        let err = normalize_usage(&json!({}), &test_credentials()).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::Parse);
    }

    fn test_credentials() -> ClaudeCredentials {
        parse_credentials(
            r#"{
                "claudeAiOauth": {
                    "accessToken": "access",
                    "refreshToken": "refresh",
                    "expiresAt": 1780000000000,
                    "scopes": ["user:inference"],
                    "subscriptionType": "team",
                    "rateLimitTier": "default"
                }
            }"#,
            super::super::credentials::CLAUDE_KEYCHAIN_SERVICE,
            "joey",
            CredentialSource::Keychain,
        )
        .unwrap()
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
