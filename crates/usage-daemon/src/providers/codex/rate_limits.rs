//! Rate-limit and credit normalization for app-server and WHAM payloads.

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{ProviderError, ProviderErrorKind, ProviderUsage};

use super::{MAX_PERCENT, PROVIDER_ID};

pub(super) fn normalize_usage(
    payload: &Value,
    display_name: Option<&str>,
) -> Result<ProviderUsage, ProviderError> {
    let object = payload.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Codex usage response was not a JSON object",
        )
    })?;

    let mut windows = collect_codex_rate_limit_windows(RateLimitGroupSpec {
        id_prefix: "codex",
        label_prefix: "Codex",
        rate_limit: object.get("rate_limit"),
    });
    windows.extend(collect_codex_rate_limit_windows(RateLimitGroupSpec {
        id_prefix: "codex_code_review",
        label_prefix: "Codex code review",
        rate_limit: object.get("code_review_rate_limit"),
    }));
    windows.extend(collect_additional_rate_limit_windows(
        object.get("additional_rate_limits"),
    ));
    windows.extend(collect_credits_window(object.get("credits")));

    let reset_credits = object.get("rate_limit_reset_credits");
    let top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "account_display_name": display_name,
            "email": object.get("email").and_then(Value::as_str),
            "collection_mode": "wham_usage_api",
            "credits_has_credits": object.get("credits").and_then(|value| value.get("has_credits")).and_then(Value::as_bool),
            "credits_overage_limit_reached": object.get("credits").and_then(|value| value.get("overage_limit_reached")).and_then(Value::as_bool),
            "credits_unlimited": object.get("credits").and_then(|value| value.get("unlimited")).and_then(Value::as_bool),
            "plan_type": object.get("plan_type").and_then(Value::as_str),
            "rate_limit_reached_type": object.get("rate_limit_reached_type").and_then(Value::as_str),
            "rate_limit_reset_credits_available_count": reset_credits
                .and_then(|value| value.get("available_count"))
                .and_then(number_from_json_value),
            "rate_limit_reset_credits": wham_reset_credits_metadata(reset_credits),
            "spend_control_reached": object.get("spend_control").and_then(|value| value.get("reached")).and_then(Value::as_bool),
            "top_level_keys": top_level_keys,
        }),
    })
}

fn wham_reset_credits_metadata(reset_credits: Option<&Value>) -> Value {
    let Some(reset_credits) = reset_credits.and_then(Value::as_object) else {
        return Value::Null;
    };

    // WHAM currently exposes only the available count. Keep the normalized
    // shape aligned with app-server output so downstream consumers can render
    // the useful partial result without needing access to raw diagnostics.
    json!({
        "available_count": reset_credits
            .get("available_count")
            .and_then(number_from_json_value),
        "credits": [],
        "next_expires_at": Value::Null,
        "next_expires_at_iso": Value::Null,
    })
}

pub(super) fn normalize_app_server_usage(
    payload: &Value,
    display_name: Option<&str>,
) -> Result<ProviderUsage, ProviderError> {
    let rate_limits_read = payload
        .get("rate_limits_read")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Codex app-server rate limits response was not a JSON object",
            )
        })?;
    let account = payload
        .get("account_read")
        .and_then(|value| value.get("account"));
    let main_rate_limit = rate_limits_read
        .get("rateLimitsByLimitId")
        .and_then(|value| value.get("codex"))
        .or_else(|| rate_limits_read.get("rateLimits"));

    let mut windows = collect_app_server_rate_limit_windows(AppServerRateLimitGroupSpec {
        id_prefix: "codex",
        label_prefix: "Codex",
        rate_limit: main_rate_limit,
    });
    windows.extend(collect_app_server_additional_rate_limit_windows(
        rate_limits_read.get("rateLimitsByLimitId"),
    ));
    windows.extend(collect_credits_window(
        main_rate_limit.and_then(|value| value.get("credits")),
    ));

    let reset_credits = rate_limits_read.get("rateLimitResetCredits");
    let top_level_keys = rate_limits_read.keys().cloned().collect::<Vec<_>>();
    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "account_display_name": display_name,
            "email": account.and_then(|value| value.get("email")).and_then(Value::as_str),
            "collection_mode": "codex_app_server_rate_limits",
            "credits_has_credits": main_rate_limit
                .and_then(|value| value.get("credits"))
                .and_then(|value| value.get("hasCredits"))
                .and_then(Value::as_bool),
            "credits_overage_limit_reached": Value::Null,
            "credits_unlimited": main_rate_limit
                .and_then(|value| value.get("credits"))
                .and_then(|value| value.get("unlimited"))
                .and_then(Value::as_bool),
            "plan_type": account
                .and_then(|value| value.get("planType"))
                .and_then(Value::as_str)
                .or_else(|| main_rate_limit.and_then(|value| value.get("planType")).and_then(Value::as_str)),
            "rate_limit_reached_type": main_rate_limit
                .and_then(|value| value.get("rateLimitReachedType"))
                .and_then(Value::as_str),
            "rate_limit_reset_credits_available_count": reset_credits
                .and_then(|value| value.get("availableCount"))
                .and_then(number_from_json_value),
            "rate_limit_reset_credits": app_server_reset_credits_metadata(reset_credits),
            "spend_control_reached": Value::Null,
            "top_level_keys": top_level_keys,
        }),
    })
}

#[derive(Clone, Debug)]
pub(super) struct RateLimitGroupSpec<'a> {
    id_prefix: &'a str,
    label_prefix: &'a str,
    rate_limit: Option<&'a Value>,
}

pub(super) struct RateLimitWindowSpec<'a> {
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    value: Option<&'a Value>,
}

pub(super) struct AppServerRateLimitGroupSpec<'a> {
    id_prefix: &'a str,
    label_prefix: &'a str,
    rate_limit: Option<&'a Value>,
}

pub(super) struct AppServerRateLimitWindowSpec<'a> {
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    value: Option<&'a Value>,
}

fn collect_codex_rate_limit_windows(spec: RateLimitGroupSpec<'_>) -> Vec<UsageWindow> {
    let Some(rate_limit) = spec.rate_limit.and_then(Value::as_object) else {
        return Vec::new();
    };

    [
        rate_limit_window(RateLimitWindowSpec {
            window_id: format!("{}_session", spec.id_prefix),
            label: format!("{} session", spec.label_prefix),
            kind: UsageWindowKind::Session,
            value: rate_limit.get("primary_window"),
        }),
        rate_limit_window(RateLimitWindowSpec {
            window_id: format!("{}_weekly", spec.id_prefix),
            label: format!("{} weekly", spec.label_prefix),
            kind: UsageWindowKind::Weekly,
            value: rate_limit.get("secondary_window"),
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn collect_app_server_rate_limit_windows(
    spec: AppServerRateLimitGroupSpec<'_>,
) -> Vec<UsageWindow> {
    let Some(rate_limit) = spec.rate_limit.and_then(Value::as_object) else {
        return Vec::new();
    };

    [
        app_server_rate_limit_window(AppServerRateLimitWindowSpec {
            window_id: format!("{}_session", spec.id_prefix),
            label: format!("{} session", spec.label_prefix),
            kind: UsageWindowKind::Session,
            value: rate_limit.get("primary"),
        }),
        app_server_rate_limit_window(AppServerRateLimitWindowSpec {
            window_id: format!("{}_weekly", spec.id_prefix),
            label: format!("{} weekly", spec.label_prefix),
            kind: UsageWindowKind::Weekly,
            value: rate_limit.get("secondary"),
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn collect_additional_rate_limit_windows(value: Option<&Value>) -> Vec<UsageWindow> {
    let Some(rate_limits) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    rate_limits
        .iter()
        .enumerate()
        .filter_map(|(index, rate_limit)| {
            let rate_limit = rate_limit.as_object()?;
            let label = rate_limit
                .get("limit_name")
                .and_then(Value::as_str)
                .or_else(|| rate_limit.get("metered_feature").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Codex additional limit {}", index + 1));
            let id_prefix = format!("codex_additional_{index}");
            Some(collect_codex_rate_limit_windows(RateLimitGroupSpec {
                id_prefix: &id_prefix,
                label_prefix: &label,
                rate_limit: rate_limit.get("rate_limit"),
            }))
        })
        .flatten()
        .collect()
}

fn collect_app_server_additional_rate_limit_windows(value: Option<&Value>) -> Vec<UsageWindow> {
    let Some(rate_limits) = value.and_then(Value::as_object) else {
        return Vec::new();
    };

    rate_limits
        .iter()
        .filter(|(limit_id, _)| limit_id.as_str() != "codex")
        .enumerate()
        .flat_map(|(index, (limit_id, rate_limit))| {
            let label = rate_limit
                .get("limitName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(limit_id);
            let id_prefix = format!("codex_additional_{index}");
            collect_app_server_rate_limit_windows(AppServerRateLimitGroupSpec {
                id_prefix: &id_prefix,
                label_prefix: label,
                rate_limit: Some(rate_limit),
            })
        })
        .collect()
}

fn collect_credits_window(credits: Option<&Value>) -> Option<UsageWindow> {
    let credits = credits.and_then(Value::as_object)?;

    let balance = credits.get("balance").and_then(number_from_json_value);
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if balance.is_none() && !unlimited {
        return None;
    }

    let remaining = if unlimited { None } else { balance };
    let mut metadata_label = "Codex credits".to_string();
    if unlimited {
        metadata_label.push_str(" (unlimited)");
    }

    Some(UsageWindow {
        window_id: "codex_credits".to_string(),
        label: metadata_label,
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: remaining.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    })
}

fn rate_limit_window(spec: RateLimitWindowSpec<'_>) -> Option<UsageWindow> {
    let object = spec.value?.as_object()?;
    let percent_used = object
        .get("used_percent")
        .and_then(number_from_json_value)
        .map(|value| value.clamp(0.0, MAX_PERCENT));
    let percent_remaining = percent_used.map(|value| MAX_PERCENT - value);
    let reset_at = object
        .get("reset_at")
        .and_then(unix_timestamp_from_json_value)
        .or_else(|| {
            object
                .get("reset_after_seconds")
                .and_then(number_from_json_value)
                .and_then(|seconds| TimeDelta::try_seconds(seconds.round() as i64))
                .map(|duration| Utc::now() + duration)
        });

    if percent_used.is_none() && reset_at.is_none() {
        return None;
    }

    Some(UsageWindow {
        window_id: spec.window_id,
        label: spec.label,
        kind: spec.kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used,
        percent_remaining,
        reset_at,
    })
}

fn app_server_rate_limit_window(spec: AppServerRateLimitWindowSpec<'_>) -> Option<UsageWindow> {
    let object = spec.value?.as_object()?;
    let percent_used = object
        .get("usedPercent")
        .and_then(number_from_json_value)
        .map(|value| value.clamp(0.0, MAX_PERCENT));
    let percent_remaining = percent_used.map(|value| MAX_PERCENT - value);
    let reset_at = object
        .get("resetsAt")
        .and_then(unix_timestamp_from_json_value);

    if percent_used.is_none() && reset_at.is_none() {
        return None;
    }

    Some(UsageWindow {
        window_id: spec.window_id,
        label: spec.label,
        kind: spec.kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used,
        percent_remaining,
        reset_at,
    })
}

fn app_server_reset_credits_metadata(reset_credits: Option<&Value>) -> Value {
    let Some(reset_credits) = reset_credits.and_then(Value::as_object) else {
        return Value::Null;
    };

    let credits = reset_credits
        .get("credits")
        .and_then(Value::as_array)
        .map(|credits| {
            credits
                .iter()
                .filter_map(app_server_reset_credit_metadata)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let next_expires_at = credits
        .iter()
        .filter(|credit| {
            credit
                .get("status")
                .and_then(Value::as_str)
                .is_none_or(|status| status == "available")
        })
        .filter_map(|credit| credit.get("expires_at").and_then(number_from_json_value))
        .min_by(|left, right| left.total_cmp(right));
    let next_expires_at_iso = next_expires_at.and_then(unix_seconds_iso);

    json!({
        "available_count": reset_credits
            .get("availableCount")
            .and_then(number_from_json_value),
        "credits": credits,
        "next_expires_at": next_expires_at,
        "next_expires_at_iso": next_expires_at_iso,
    })
}

fn app_server_reset_credit_metadata(credit: &Value) -> Option<Value> {
    let credit = credit.as_object()?;
    let granted_at = credit.get("grantedAt").and_then(number_from_json_value);
    let expires_at = credit.get("expiresAt").and_then(number_from_json_value);
    Some(json!({
        "id": credit.get("id").and_then(Value::as_str),
        "status": credit.get("status").and_then(Value::as_str),
        "reset_type": credit.get("resetType").and_then(Value::as_str),
        "granted_at": granted_at,
        "granted_at_iso": granted_at.and_then(unix_seconds_iso),
        "expires_at": expires_at,
        "expires_at_iso": expires_at.and_then(unix_seconds_iso),
        "title": credit.get("title").and_then(Value::as_str),
        "description": credit.get("description").and_then(Value::as_str),
    }))
}

fn unix_seconds_iso(seconds: f64) -> Option<String> {
    DateTime::from_timestamp(seconds.round() as i64, 0).map(|time| time.to_rfc3339())
}

pub(super) fn number_from_json_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(value) => value.parse().ok().filter(|value: &f64| value.is_finite()),
        _ => None,
    }
}

fn unix_timestamp_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    let seconds = number_from_json_value(value)?.round() as i64;
    DateTime::from_timestamp(seconds, 0)
}
