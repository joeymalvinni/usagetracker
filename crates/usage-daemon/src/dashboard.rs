use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::Value;
use usage_core::{
    AccountUsageSummary, ActivitySummary, CostSummary, DailyUsagePoint, DataProvenance,
    PricingCoverage, ResetCredit, ResetCreditSummary, UsageDashboardSummary, UsageDataCompleteness,
    UsageDataConfidence, UsageDataQuality, UsageDataScope, UsageDataSource, UsageSnapshot,
};

pub(crate) fn build_usage_dashboard(snapshots: &[UsageSnapshot]) -> UsageDashboardSummary {
    let codex_reference_rate = codex_reference_cost_per_token(snapshots);
    let accounts = snapshots
        .iter()
        .filter_map(|snapshot| account_summary(snapshot, codex_reference_rate))
        .collect::<Vec<_>>();
    usage_core::aggregate_usage_dashboard(accounts)
}

fn account_summary(
    snapshot: &UsageSnapshot,
    codex_reference_rate: Option<f64>,
) -> Option<AccountUsageSummary> {
    let provider_id = snapshot.provider_id.as_str();
    let cost = snapshot
        .metadata
        .get(format!("{provider_id}_cost"))
        .and_then(Value::as_object);
    let activity = snapshot
        .metadata
        .get(format!("{provider_id}_activity"))
        .and_then(Value::as_object);
    if cost.is_none() && activity.is_none() {
        return None;
    }

    let cost_source = cost
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str);
    let activity_source = activity
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
        .or(cost_source);
    let mut cost_days = cost
        .and_then(|value| value.get("by_day"))
        .and_then(Value::as_array)
        .map(|rows| daily_points(rows))
        .unwrap_or_else(|| synthesized_today_point(cost));
    let activity_days = activity
        .and_then(|value| value.get("by_day"))
        .and_then(Value::as_array)
        .map(|rows| daily_points(rows))
        .unwrap_or_else(|| cost_days.clone());

    if provider_id == "codex" && !activity_days.is_empty() {
        cost_days = extrapolate_codex_costs(&activity_days, &cost_days, codex_reference_rate);
    }

    let activity_summary = (!activity_days.is_empty()).then(|| {
        let today = Local::now().date_naive();
        ActivitySummary {
            provenance: provenance_for(activity_source, false, activity),
            today_tokens: activity_days
                .iter()
                .find(|point| point.date == today)
                .map_or(0, |point| point.tokens),
            lookback_tokens: activity_days
                .iter()
                .fold(0_u64, |total, point| total.saturating_add(point.tokens)),
            lifetime_tokens: activity
                .and_then(|value| value.get("lifetime_tokens"))
                .and_then(Value::as_u64),
            days: activity_days,
        }
    });

    let cost_summary = cost.map(|metadata| {
        let priced_tokens = cost_days.iter().fold(0_u64, |total, point| {
            total.saturating_add(point.priced_tokens)
        });
        let unpriced_tokens = cost_days.iter().fold(0_u64, |total, point| {
            total.saturating_add(point.unpriced_tokens)
        });
        let today = Local::now().date_naive();
        CostSummary {
            provenance: provenance_for(cost_source, true, Some(metadata)),
            today_cost_usd: cost_days
                .iter()
                .find(|point| point.date == today)
                .and_then(|point| point.cost_usd)
                .unwrap_or(0.0),
            lookback_cost_usd: cost_days.iter().filter_map(|point| point.cost_usd).sum(),
            pricing: PricingCoverage {
                priced_tokens,
                unpriced_tokens,
                covered_percent: covered_percent(priced_tokens, unpriced_tokens),
                unpriced_models: unpriced_models(metadata),
                catalog_version: string(metadata, "pricing_version"),
                catalog_source: string(metadata, "pricing_source"),
                fetched_at: string(metadata, "pricing_fetched_at")
                    .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                    .map(|value| value.with_timezone(&Utc)),
                catalog_effective_from: string(metadata, "pricing_effective_from")
                    .and_then(|value| NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok()),
            },
            days: cost_days,
        }
    });

    Some(AccountUsageSummary {
        provider_id: snapshot.provider_id.clone(),
        account_id: snapshot.account_id.clone(),
        activity: activity_summary,
        cost: cost_summary,
        reset_credits: reset_credit_summary(snapshot),
    })
}

fn reset_credit_summary(snapshot: &UsageSnapshot) -> Option<ResetCreditSummary> {
    let root = snapshot.metadata.get("rate_limit_reset_credits")?;
    let available_count = root
        .get("available_count")
        .and_then(nonnegative_integer)
        .or_else(|| {
            snapshot
                .metadata
                .get("rate_limit_reset_credits_available_count")
                .and_then(nonnegative_integer)
        })
        .unwrap_or(0);
    let credits = root
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, value)| {
            let value = value.as_object()?;
            Some(ResetCredit {
                id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}:reset:{index}", snapshot.account_id)),
                title: value
                    .get("title")
                    .or_else(|| value.get("reset_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("Reset credit")
                    .to_string(),
                status: value
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                expires_at: timestamp(value, "expires_at", "expires_at_iso"),
            })
        })
        .collect::<Vec<_>>();
    let next_expires_at = root
        .as_object()
        .and_then(|value| timestamp(value, "next_expires_at", "next_expires_at_iso"));
    (available_count > 0 || !credits.is_empty()).then_some(ResetCreditSummary {
        available_count,
        next_expires_at,
        credits,
    })
}

fn nonnegative_integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let value = value.as_f64()?;
        (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64)
            .then_some(value as u64)
    })
}

fn timestamp(
    value: &serde_json::Map<String, Value>,
    seconds_key: &str,
    iso_key: &str,
) -> Option<DateTime<Utc>> {
    value
        .get(iso_key)
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            value
                .get(seconds_key)
                .and_then(Value::as_f64)
                .and_then(|seconds| DateTime::from_timestamp(seconds as i64, 0))
        })
}

fn extrapolate_codex_costs(
    activity: &[DailyUsagePoint],
    local_cost: &[DailyUsagePoint],
    reference_rate: Option<f64>,
) -> Vec<DailyUsagePoint> {
    let local = local_cost
        .iter()
        .map(|point| (point.date, point))
        .collect::<BTreeMap<_, _>>();
    activity
        .iter()
        .map(|activity| {
            let local = local.get(&activity.date).copied();
            let estimated_cost = local
                .filter(|point| point.cost_usd.unwrap_or(0.0) > 0.0 && point.tokens > 0)
                .map(|point| {
                    let local_cost = point.cost_usd.unwrap_or(0.0);
                    if activity.tokens <= point.tokens {
                        local_cost * activity.tokens as f64 / point.tokens as f64
                    } else if point.priced_tokens > 0 {
                        local_cost
                            + activity.tokens.saturating_sub(point.tokens) as f64 * local_cost
                                / point.priced_tokens as f64
                    } else {
                        local_cost
                    }
                })
                .or_else(|| reference_rate.map(|rate| activity.tokens as f64 * rate));
            let priced_tokens = local.map_or(0, |point| point.priced_tokens.min(activity.tokens));
            DailyUsagePoint {
                date: activity.date,
                tokens: activity.tokens,
                cost_usd: estimated_cost,
                priced_tokens,
                unpriced_tokens: activity.tokens.saturating_sub(priced_tokens),
            }
        })
        .collect()
}

fn codex_reference_cost_per_token(snapshots: &[UsageSnapshot]) -> Option<f64> {
    let mut total_cost = 0.0;
    let mut priced_tokens = 0_u64;
    for snapshot in snapshots
        .iter()
        .filter(|snapshot| snapshot.provider_id.as_str() == "codex")
    {
        let Some(cost) = snapshot.metadata.get("codex_cost") else {
            continue;
        };
        let cost_usd = number(cost, "total_cost_usd").unwrap_or(0.0);
        let tokens = integer(cost, "priced_tokens")
            .or_else(|| integer(cost, "total_tokens"))
            .unwrap_or(0);
        if cost_usd > 0.0 && tokens > 0 {
            total_cost += cost_usd;
            priced_tokens = priced_tokens.saturating_add(tokens);
        }
    }
    (total_cost > 0.0 && priced_tokens > 0).then(|| total_cost / priced_tokens as f64)
}

fn daily_points(rows: &[Value]) -> Vec<DailyUsagePoint> {
    let mut by_date = BTreeMap::new();
    for row in rows.iter().filter_map(Value::as_object) {
        let Some(date) = row
            .get("date")
            .and_then(Value::as_str)
            .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
        else {
            continue;
        };
        let tokens = row.get("tokens").and_then(Value::as_u64).unwrap_or(0);
        let unpriced_tokens = row
            .get("unpriced_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(tokens);
        let priced_tokens = row
            .get("priced_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| tokens.saturating_sub(unpriced_tokens))
            .min(tokens);
        by_date.insert(
            date,
            DailyUsagePoint {
                date,
                tokens,
                cost_usd: row.get("cost_usd").and_then(Value::as_f64),
                priced_tokens,
                unpriced_tokens,
            },
        );
    }
    by_date.into_values().collect()
}

fn synthesized_today_point(
    metadata: Option<&serde_json::Map<String, Value>>,
) -> Vec<DailyUsagePoint> {
    let Some(metadata) = metadata else {
        return Vec::new();
    };
    let tokens = metadata
        .get("today_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cost = metadata.get("today_cost_usd").and_then(Value::as_f64);
    if tokens == 0 && cost.unwrap_or(0.0) == 0.0 {
        return Vec::new();
    }
    let unpriced_tokens = metadata
        .get("unpriced_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(tokens);
    vec![DailyUsagePoint {
        date: Local::now().date_naive(),
        tokens,
        cost_usd: cost,
        priced_tokens: tokens.saturating_sub(unpriced_tokens),
        unpriced_tokens,
    }]
}

fn provenance_for(
    source: Option<&str>,
    cost: bool,
    metadata: Option<&serde_json::Map<String, Value>>,
) -> DataProvenance {
    let (source, scope, default_partial) = match source.unwrap_or_default() {
        "local_session_logs" => (UsageDataSource::LocalLogs, UsageDataScope::ThisDevice, true),
        "local_project_logs" => (
            UsageDataSource::LocalLogs,
            UsageDataScope::SelectedLocalRoots,
            true,
        ),
        "opencode_local_sqlite" => (
            UsageDataSource::LocalDatabase,
            UsageDataScope::ThisDevice,
            true,
        ),
        "opencode_usage_page" => (
            UsageDataSource::ProviderReported,
            UsageDataScope::Workspace,
            true,
        ),
        _ => (
            UsageDataSource::ProviderReported,
            UsageDataScope::AccountWide,
            false,
        ),
    };
    let marked_partial = metadata
        .and_then(|value| value.get("partial"))
        .and_then(Value::as_bool)
        .unwrap_or(default_partial)
        || !metadata
            .and_then(|value| value.get("complete_lookback"))
            .and_then(Value::as_bool)
            .unwrap_or(!default_partial);
    DataProvenance {
        source,
        scope,
        quality: if cost
            || metadata
                .and_then(|value| value.get("estimate"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            UsageDataQuality::Estimated
        } else if source == UsageDataSource::ProviderReported {
            UsageDataQuality::Authoritative
        } else {
            UsageDataQuality::Observed
        },
        completeness: if marked_partial {
            UsageDataCompleteness::Partial
        } else {
            UsageDataCompleteness::Complete
        },
        confidence: if source == UsageDataSource::ProviderReported {
            UsageDataConfidence::High
        } else {
            UsageDataConfidence::Medium
        },
    }
}

fn covered_percent(priced: u64, unpriced: u64) -> f64 {
    let total = priced.saturating_add(unpriced);
    if total == 0 {
        0.0
    } else {
        (priced as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn unpriced_models(metadata: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut models = BTreeSet::new();
    for value in metadata
        .get("unpriced_models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(model) = value
            .as_str()
            .or_else(|| value.get("model").and_then(Value::as_str))
        {
            models.insert(model.to_string());
        }
    }
    models.into_iter().collect()
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn integer(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn string(metadata: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use usage_core::{AccountId, ProviderId};

    #[test]
    fn builds_typed_mixed_scope_dashboard_and_honest_pricing_coverage() {
        let collected_at = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
        let snapshots = vec![
            UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("codex-account"),
                collected_at,
                windows: Vec::new(),
                metadata: json!({
                    "codex_activity": {"source":"codex_account_usage","by_day":[{"date":"2026-07-11","tokens":1000}]},
                    "codex_cost": {"source":"local_session_logs","estimate":true,"partial":true,"by_day":[{"date":"2026-07-11","tokens":400,"priced_tokens":300,"unpriced_tokens":100,"cost_usd":1.5}]}
                }),
            },
            UsageSnapshot {
                provider_id: ProviderId::new("claude"),
                account_id: AccountId::new("claude-account"),
                collected_at,
                windows: Vec::new(),
                metadata: json!({
                    "claude_cost": {"source":"local_project_logs","estimate":true,"by_day":[{"date":"2026-07-11","tokens":200,"priced_tokens":200,"cost_usd":0.5}]}
                }),
            },
        ];

        let dashboard = build_usage_dashboard(&snapshots);

        assert_eq!(dashboard.accounts.len(), 2);
        assert!(dashboard.provenance.mixed_scope);
        assert!(dashboard.provenance.partial);
        assert!(dashboard.provenance.estimated);
        assert_eq!(dashboard.days[0].tokens, 1_200);
        assert_eq!(dashboard.pricing.priced_tokens, 500);
        assert_eq!(dashboard.pricing.unpriced_tokens, 700);
        assert!(dashboard
            .provenance
            .explanation
            .contains("not directly comparable"));
    }

    #[test]
    fn reset_credit_summary_accepts_normalized_float_count() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            metadata: json!({
                "rate_limit_reset_credits": {
                    "available_count": 3.0,
                    "next_expires_at": 1784336002.0,
                    "credits": [
                        {
                            "id": "reset-1",
                            "title": "Full reset",
                            "status": "available",
                            "expires_at": 1784336002.0
                        }
                    ]
                }
            }),
        };

        let summary = reset_credit_summary(&snapshot).expect("reset credit summary");

        assert_eq!(summary.available_count, 3);
        assert_eq!(summary.credits.len(), 1);
    }
}
