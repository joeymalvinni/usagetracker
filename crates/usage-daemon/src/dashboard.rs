use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::Value;
use usage_core::{
    AccountUsageSummary, ActivitySummary, CostSummary, DailyUsagePoint, DataProvenance,
    ModelCostSummary, PricingCoverage, ResetCredit, ResetCreditSummary, UsageDashboardSummary,
    UsageDataCompleteness, UsageDataConfidence, UsageDataQuality, UsageDataScope, UsageDataSource,
    UsageSnapshot,
};

use crate::storage::StoredDailyUsageHistory;

pub(crate) fn build_usage_dashboard(
    snapshots: &[UsageSnapshot],
    daily_usage: &[StoredDailyUsageHistory],
) -> UsageDashboardSummary {
    let history = daily_usage
        .iter()
        .map(|history| {
            (
                (history.provider_id.as_str(), history.account_id.as_str()),
                history,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let accounts = snapshots
        .iter()
        .filter_map(|snapshot| {
            account_summary(
                snapshot,
                history
                    .get(&(snapshot.provider_id.as_str(), snapshot.account_id.as_str()))
                    .copied(),
            )
        })
        .collect::<Vec<_>>();
    usage_core::aggregate_usage_dashboard(accounts)
}

fn account_summary(
    snapshot: &UsageSnapshot,
    retained_activity: Option<&StoredDailyUsageHistory>,
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
    let reset_credits = reset_credit_summary(snapshot);
    if cost.is_none()
        && activity.is_none()
        && retained_activity.is_none()
        && reset_credits.is_none()
    {
        return None;
    }

    let cost_source = cost
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str);
    let cost_rows = cost
        .and_then(|value| value.get("by_day"))
        .and_then(Value::as_array);
    let cost_days = cost_rows
        .map(|rows| daily_points(rows))
        .unwrap_or_else(|| synthesized_today_point(cost));
    let (activity_source, activity_metadata, activity_days, activity_lifetime_tokens) =
        if provider_id == "codex" {
            // Codex's account endpoint reports an opaque processed-token total. Use
            // local logs for a cost-aligned activity graph that includes cached input
            // once, and keep account-wide data as diagnostics only.
            (
                cost_source,
                cost,
                cost_rows.map(|rows| daily_points(rows)).unwrap_or_default(),
                cost.and_then(|value| value.get("total_tokens"))
                    .and_then(Value::as_u64),
            )
        } else {
            let source = retained_activity
                .and_then(|history| history.recent.last())
                .map(|row| row.source.as_str())
                .or_else(|| {
                    activity
                        .and_then(|value| value.get("source"))
                        .and_then(Value::as_str)
                })
                .or(cost_source);
            let days = retained_activity
                .map(|history| {
                    history
                        .recent
                        .iter()
                        .map(|row| DailyUsagePoint {
                            date: row.date,
                            tokens: row.tokens,
                            cost_usd: row.cost_usd,
                            priced_tokens: 0,
                            unpriced_tokens: 0,
                        })
                        .collect()
                })
                .or_else(|| {
                    activity
                        .and_then(|value| value.get("by_day"))
                        .and_then(Value::as_array)
                        .map(|rows| daily_points(rows))
                })
                .unwrap_or_else(|| cost_days.clone());
            let lifetime_tokens = retained_activity
                .map(|history| history.total_tokens)
                .or_else(|| {
                    activity
                        .and_then(|value| value.get("lifetime_tokens"))
                        .and_then(Value::as_u64)
                });
            (source, activity, days, lifetime_tokens)
        };

    let activity_summary = (!activity_days.is_empty()).then(|| {
        let today = Local::now().date_naive();
        ActivitySummary {
            provenance: typed_or_legacy_provenance(
                snapshot,
                activity_source,
                false,
                activity_metadata,
            ),
            today_tokens: activity_days
                .iter()
                .find(|point| point.date == today)
                .map_or(0, |point| point.tokens),
            lookback_tokens: activity_days
                .iter()
                .fold(0_u64, |total, point| total.saturating_add(point.tokens)),
            lifetime_tokens: activity_lifetime_tokens,
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
            provenance: typed_or_legacy_provenance(snapshot, cost_source, true, Some(metadata)),
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
                catalog_effective_from: string(metadata, "pricing_effective_from")
                    .and_then(|value| NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok()),
            },
            models: model_costs(metadata),
            days: cost_days,
        }
    });

    Some(AccountUsageSummary {
        provider_id: snapshot.provider_id.clone(),
        account_id: snapshot.account_id.clone(),
        activity: activity_summary,
        cost: cost_summary,
        reset_credits,
    })
}

fn reset_credit_summary(snapshot: &UsageSnapshot) -> Option<ResetCreditSummary> {
    let root = snapshot
        .metadata
        .get("rate_limit_reset_credits")
        .and_then(Value::as_object);
    let available_count = root
        .and_then(|value| value.get("available_count"))
        .and_then(nonnegative_integer)
        .or_else(|| {
            snapshot
                .metadata
                .get("rate_limit_reset_credits_available_count")
                .and_then(nonnegative_integer)
        })
        .unwrap_or(0);
    let credits = root
        .and_then(|value| value.get("credits"))
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
    let next_expires_at =
        root.and_then(|value| timestamp(value, "next_expires_at", "next_expires_at_iso"));
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
                cost_usd: row
                    .get("cost_usd")
                    .or_else(|| row.get("metered_cost_usd"))
                    .and_then(Value::as_f64),
                priced_tokens,
                unpriced_tokens,
            },
        );
    }
    by_date.into_values().collect()
}

fn model_costs(metadata: &serde_json::Map<String, Value>) -> Vec<ModelCostSummary> {
    metadata
        .get("by_model")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|row| {
            Some(ModelCostSummary {
                model: row.get("model")?.as_str()?.to_string(),
                event_count: row.get("event_count")?.as_u64()?,
                tokens: row.get("tokens")?.as_u64()?,
                vendor_cost_usd: row.get("vendor_cost_usd")?.as_f64()?,
                metered_cost_usd: row.get("metered_cost_usd")?.as_f64()?,
                chargeable_cost_usd: row.get("chargeable_cost_usd")?.as_f64()?,
                provider_fee_usd: row.get("provider_fee_usd")?.as_f64()?,
            })
        })
        .collect()
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

fn typed_or_legacy_provenance(
    snapshot: &UsageSnapshot,
    source: Option<&str>,
    cost: bool,
    metadata: Option<&serde_json::Map<String, Value>>,
) -> DataProvenance {
    source
        .and_then(|source| snapshot.daily_provenance(source))
        .unwrap_or_else(|| legacy_provenance_for(source, cost, metadata))
}

/// Compatibility for snapshots written before datasets carried typed source
/// mappings. New providers never need to add cases here.
fn legacy_provenance_for(
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

    use crate::storage::StoredDailyUsage;

    #[test]
    fn codex_dashboard_uses_local_processed_activity_without_scaling_cost() {
        let collected_at = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
        let snapshots = vec![
            UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("codex-account"),
                collected_at,
                windows: Vec::new(),
                metadata: json!({
                    "codex_activity": {"source":"codex_account_usage","by_day":[{"date":"2026-07-11","tokens":1_500_000_000}]},
                    "codex_cost": {
                        "source":"local_session_logs",
                        "estimate":true,
                        "partial":true,
                        "total_tokens":1_400_000_100_u64,
                        "total_activity_tokens":100,
                        "by_day":[{
                            "date":"2026-07-11",
                            "tokens":1_400_000_100_u64,
                            "activity_tokens":100,
                            "cached_input_tokens":1_400_000_000_u64,
                            "priced_tokens":1_400_000_000_u64,
                            "unpriced_tokens":100,
                            "cost_usd":1.5
                        }]
                    }
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

        let dashboard = build_usage_dashboard(&snapshots, &[]);

        assert_eq!(dashboard.accounts.len(), 2);
        assert!(dashboard.provenance.mixed_scope);
        assert!(dashboard.provenance.partial);
        assert!(dashboard.provenance.estimated);
        assert_eq!(dashboard.days[0].tokens, 1_400_000_300);
        assert_eq!(dashboard.days[0].cost_usd, Some(2.0));
        assert_eq!(dashboard.pricing.priced_tokens, 1_400_000_200);
        assert_eq!(dashboard.pricing.unpriced_tokens, 100);
        let codex = dashboard
            .accounts
            .iter()
            .find(|account| account.provider_id.as_str() == "codex")
            .unwrap();
        assert_eq!(
            codex.activity.as_ref().unwrap().lookback_tokens,
            1_400_000_100
        );
        assert_eq!(
            codex.activity.as_ref().unwrap().lifetime_tokens,
            Some(1_400_000_100)
        );
        assert_eq!(codex.cost.as_ref().unwrap().lookback_cost_usd, 1.5);
        assert!(dashboard
            .provenance
            .explanation
            .contains("not directly comparable"));
    }

    #[test]
    fn codex_account_processed_tokens_are_not_used_without_local_logs() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            metadata: json!({
                "codex_activity": {
                    "source": "codex_account_usage",
                    "by_day": [{"date": "2026-07-11", "tokens": 1_500_000_000_u64}]
                }
            }),
        };

        let dashboard = build_usage_dashboard(&[snapshot], &[]);

        assert!(dashboard.accounts[0].activity.is_none());
        assert!(dashboard.days.is_empty());
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

    #[test]
    fn reset_credit_summary_accepts_legacy_scalar_without_details() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            metadata: json!({
                "rate_limit_reset_credits_available_count": 4.0
            }),
        };

        let summary = reset_credit_summary(&snapshot).expect("reset credit summary");

        assert_eq!(summary.available_count, 4);
        assert!(summary.next_expires_at.is_none());
        assert!(summary.credits.is_empty());

        let dashboard = build_usage_dashboard(&[snapshot], &[]);
        assert_eq!(dashboard.accounts.len(), 1);
        assert_eq!(
            dashboard.accounts[0]
                .reset_credits
                .as_ref()
                .map(|summary| summary.available_count),
            Some(4)
        );
    }

    #[test]
    fn retained_daily_activity_overrides_stale_snapshot_activity() {
        let provider_id = ProviderId::new("opencode_go");
        let account_id = AccountId::new("account");
        let retained_date = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        let snapshot = UsageSnapshot {
            provider_id: provider_id.clone(),
            account_id: account_id.clone(),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            metadata: json!({
                "opencode_go_activity": {
                    "source": "provider_reported",
                    "lifetime_tokens": 1,
                    "by_day": [{"date": "2026-07-09", "tokens": 1}]
                }
            }),
        };
        let retained = StoredDailyUsageHistory {
            provider_id,
            account_id,
            bucket_count: 4,
            total_tokens: 150,
            recent: vec![StoredDailyUsage {
                provider_id: ProviderId::new("opencode_go"),
                account_id: AccountId::new("account"),
                date: retained_date,
                tokens: 75,
                cost_usd: Some(0.25),
                source: "opencode_local_sqlite".to_string(),
            }],
        };

        let dashboard = build_usage_dashboard(&[snapshot], &[retained]);

        let activity = dashboard.accounts[0].activity.as_ref().unwrap();
        assert_eq!(activity.provenance.source, UsageDataSource::LocalDatabase);
        assert_eq!(activity.lifetime_tokens, Some(150));
        assert_eq!(activity.lookback_tokens, 75);
        assert_eq!(activity.days.len(), 1);
        assert_eq!(activity.days[0].date, retained_date);
        assert_eq!(activity.days[0].tokens, 75);
        assert_eq!(activity.days[0].cost_usd, Some(0.25));
    }

    #[test]
    fn dashboard_uses_typed_daily_provenance_without_knowing_provider_labels() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("future_provider"),
            account_id: AccountId::new("account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            metadata: json!({
                "future_provider_activity": {
                    "source": "future_stream_v9",
                    "by_day": [{"date": "2026-07-11", "tokens": 42}]
                },
                "dataset_provenance": [{
                    "authoritative": false,
                    "provenance": {
                        "source": "local_database",
                        "scope": "workspace",
                        "quality": "observed",
                        "completeness": "complete",
                        "confidence": "medium"
                    },
                    "daily_sources": ["future_stream_v9"]
                }]
            }),
        };

        let dashboard = build_usage_dashboard(&[snapshot], &[]);
        let provenance = &dashboard.accounts[0].activity.as_ref().unwrap().provenance;

        assert_eq!(provenance.source, UsageDataSource::LocalDatabase);
        assert_eq!(provenance.scope, UsageDataScope::Workspace);
        assert_eq!(provenance.completeness, UsageDataCompleteness::Complete);
    }
}
