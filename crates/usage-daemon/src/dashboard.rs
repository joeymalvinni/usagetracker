use std::collections::{BTreeMap, BTreeSet};

use chrono::Local;
use usage_core::{
    AccountUsageSummary, ActivitySummary, CostDetail, CostSummary, DailyUsagePoint, DataProvenance,
    PricingCoverage, ResetCredit, ResetCreditSummary, UsageDashboardSummary, UsageDataCompleteness,
    UsageDataConfidence, UsageDataQuality, UsageDataScope, UsageDataSource, UsageSnapshot,
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
    let cost = snapshot.detail.cost.as_ref();
    let activity = snapshot.detail.activity.as_ref();
    let reset_credits = reset_credit_summary(snapshot);
    if cost.is_none()
        && activity.is_none()
        && retained_activity.is_none()
        && reset_credits.is_none()
    {
        return None;
    }

    let cost_source = cost.and_then(|cost| cost.source.as_deref());
    let cost_days = cost
        .map(|cost| {
            if cost.by_day.is_empty() {
                synthesized_today_point(cost)
            } else {
                cost.by_day.clone()
            }
        })
        .unwrap_or_default();
    let activity_source = retained_activity
        .and_then(|history| history.recent.last())
        .map(|row| row.source.as_str())
        .or_else(|| activity.and_then(|activity| activity.source.as_deref()))
        .or(cost_source);
    let activity_days = retained_activity
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
                .filter(|activity| !activity.by_day.is_empty())
                .map(|activity| activity.by_day.clone())
        })
        .unwrap_or_else(|| cost_days.clone());
    let activity_lifetime_tokens = retained_activity
        .map(|history| history.total_tokens)
        .or_else(|| activity.and_then(|activity| activity.lifetime_tokens));

    let activity_summary = (!activity_days.is_empty()).then(|| {
        let today = Local::now().date_naive();
        ActivitySummary {
            provenance: activity_provenance(snapshot, activity_source),
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

    let cost_summary = cost.map(|cost| {
        let priced_tokens = cost_days.iter().fold(0_u64, |total, point| {
            total.saturating_add(point.priced_tokens)
        });
        let unpriced_tokens = cost_days.iter().fold(0_u64, |total, point| {
            total.saturating_add(point.unpriced_tokens)
        });
        let today = Local::now().date_naive();
        CostSummary {
            provenance: cost_provenance(snapshot, cost_source, cost),
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
                unpriced_models: unpriced_model_names(cost),
                catalog_version: cost.pricing_version.clone(),
                catalog_source: cost.pricing_source.clone(),
                catalog_effective_from: cost.pricing_effective_from,
            },
            models: cost.by_model.clone(),
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
    let detail = snapshot.detail.reset_credits.as_ref()?;
    let credits = detail
        .credits
        .iter()
        .enumerate()
        .map(|(index, credit)| ResetCredit {
            id: credit
                .id
                .clone()
                .unwrap_or_else(|| format!("{}:reset:{index}", snapshot.account_id)),
            title: credit
                .title
                .clone()
                .unwrap_or_else(|| "Reset credit".to_string()),
            status: credit
                .status
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            expires_at: credit.expires_at,
        })
        .collect::<Vec<_>>();
    (detail.available_count > 0 || !credits.is_empty()).then_some(ResetCreditSummary {
        available_count: detail.available_count,
        next_expires_at: detail.next_expires_at,
        credits,
    })
}

fn synthesized_today_point(cost: &CostDetail) -> Vec<DailyUsagePoint> {
    let tokens = cost.today_tokens.unwrap_or(0);
    let cost_usd = cost.today_cost_usd;
    if tokens == 0 && cost_usd.unwrap_or(0.0) == 0.0 {
        return Vec::new();
    }
    let unpriced_tokens = cost.unpriced_tokens.min(tokens);
    vec![DailyUsagePoint {
        date: Local::now().date_naive(),
        tokens,
        cost_usd,
        priced_tokens: tokens.saturating_sub(unpriced_tokens),
        unpriced_tokens,
    }]
}

fn cost_provenance(
    snapshot: &UsageSnapshot,
    source: Option<&str>,
    cost: &CostDetail,
) -> DataProvenance {
    source
        .and_then(|source| snapshot.daily_provenance(source))
        .unwrap_or_else(|| {
            legacy_provenance_for(
                source,
                true,
                Some(cost.partial),
                cost.complete_lookback,
                true,
            )
        })
}

fn activity_provenance(snapshot: &UsageSnapshot, source: Option<&str>) -> DataProvenance {
    source
        .and_then(|source| snapshot.daily_provenance(source))
        .unwrap_or_else(|| legacy_provenance_for(source, false, None, None, false))
}

/// Compatibility for datasets that do not register a typed daily-source
/// mapping (e.g. local cost estimates with no daily buckets). New providers
/// never need to add cases here.
fn legacy_provenance_for(
    source: Option<&str>,
    cost: bool,
    partial: Option<bool>,
    complete_lookback: Option<bool>,
    estimate: bool,
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
    let marked_partial =
        partial.unwrap_or(default_partial) || !complete_lookback.unwrap_or(!default_partial);
    DataProvenance {
        source,
        scope,
        quality: if cost || estimate {
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

fn unpriced_model_names(cost: &CostDetail) -> Vec<String> {
    cost.unpriced_models
        .iter()
        .map(|model| model.model.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};
    use usage_core::{
        AccountId, ActivityDetail, CostDetail, DataProvenance, DatasetProvenance, ProviderId,
        ResetCreditEntry, ResetCreditsDetail, SnapshotDetail,
    };

    use crate::storage::StoredDailyUsage;

    fn day(
        date: &str,
        tokens: u64,
        priced: u64,
        unpriced: u64,
        cost: Option<f64>,
    ) -> DailyUsagePoint {
        DailyUsagePoint {
            date: NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            tokens,
            cost_usd: cost,
            priced_tokens: priced,
            unpriced_tokens: unpriced,
        }
    }

    #[test]
    fn codex_dashboard_uses_provider_activity_without_scaling_local_cost() {
        let collected_at = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
        let retained_activity = StoredDailyUsageHistory {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            bucket_count: 62,
            total_tokens: 3_000,
            recent: vec![StoredDailyUsage {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("codex-account"),
                date: NaiveDate::from_ymd_opt(2026, 7, 11).unwrap(),
                tokens: 1_000,
                cost_usd: None,
                source: "codex_account_usage".to_string(),
            }],
        };
        let snapshots = vec![
            UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new("codex-account"),
                collected_at,
                windows: Vec::new(),
                detail: SnapshotDetail {
                    activity: Some(ActivityDetail {
                        source: Some("codex_account_usage".to_string()),
                        by_day: vec![day("2026-07-11", 1_000, 0, 0, None)],
                        ..ActivityDetail::default()
                    }),
                    cost: Some(CostDetail {
                        source: Some("local_session_logs".to_string()),
                        estimate: true,
                        partial: true,
                        total_tokens: Some(400),
                        by_day: vec![day("2026-07-11", 400, 300, 100, Some(1.5))],
                        ..CostDetail::default()
                    }),
                    ..SnapshotDetail::default()
                },
            },
            UsageSnapshot {
                provider_id: ProviderId::new("claude"),
                account_id: AccountId::new("claude-account"),
                collected_at,
                windows: Vec::new(),
                detail: SnapshotDetail {
                    cost: Some(CostDetail {
                        source: Some("local_project_logs".to_string()),
                        estimate: true,
                        by_day: vec![day("2026-07-11", 200, 200, 0, Some(0.5))],
                        ..CostDetail::default()
                    }),
                    ..SnapshotDetail::default()
                },
            },
        ];

        let dashboard = build_usage_dashboard(&snapshots, &[retained_activity]);

        assert_eq!(dashboard.accounts.len(), 2);
        assert!(dashboard.provenance.mixed_scope);
        assert!(dashboard.provenance.partial);
        assert!(dashboard.provenance.estimated);
        assert_eq!(dashboard.days[0].tokens, 1_200);
        assert_eq!(dashboard.days[0].cost_usd, Some(2.0));
        assert_eq!(dashboard.pricing.priced_tokens, 500);
        assert_eq!(dashboard.pricing.unpriced_tokens, 100);
        let codex = dashboard
            .accounts
            .iter()
            .find(|account| account.provider_id.as_str() == "codex")
            .unwrap();
        assert_eq!(codex.activity.as_ref().unwrap().lookback_tokens, 1_000);
        assert_eq!(
            codex.activity.as_ref().unwrap().lifetime_tokens,
            Some(3_000)
        );
        assert_eq!(
            codex.activity.as_ref().unwrap().provenance.scope,
            UsageDataScope::AccountWide
        );
        assert_eq!(
            codex.activity.as_ref().unwrap().provenance.quality,
            UsageDataQuality::Authoritative
        );
        assert_eq!(codex.cost.as_ref().unwrap().lookback_cost_usd, 1.5);
        assert!(dashboard
            .provenance
            .explanation
            .contains("not directly comparable"));
    }

    #[test]
    fn codex_account_processed_tokens_drive_activity_without_local_logs() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            detail: SnapshotDetail {
                activity: Some(ActivityDetail {
                    source: Some("codex_account_usage".to_string()),
                    by_day: vec![day("2026-07-11", 1_500_000_000, 0, 0, None)],
                    ..ActivityDetail::default()
                }),
                ..SnapshotDetail::default()
            },
        };

        let dashboard = build_usage_dashboard(&[snapshot], &[]);

        assert_eq!(
            dashboard.accounts[0]
                .activity
                .as_ref()
                .unwrap()
                .lookback_tokens,
            1_500_000_000
        );
        assert_eq!(dashboard.days[0].tokens, 1_500_000_000);
    }

    #[test]
    fn reset_credit_summary_accepts_normalized_float_count() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            detail: SnapshotDetail {
                reset_credits: Some(ResetCreditsDetail {
                    available_count: 3,
                    next_expires_at: DateTime::from_timestamp(1_784_336_002, 0),
                    credits: vec![ResetCreditEntry {
                        id: Some("reset-1".to_string()),
                        title: Some("Full reset".to_string()),
                        status: Some("available".to_string()),
                        expires_at: DateTime::from_timestamp(1_784_336_002, 0),
                    }],
                }),
                ..SnapshotDetail::default()
            },
        };

        let summary = reset_credit_summary(&snapshot).expect("reset credit summary");

        assert_eq!(summary.available_count, 3);
        assert_eq!(summary.credits.len(), 1);
    }

    #[test]
    fn reset_credit_summary_accepts_count_without_details() {
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("codex-account"),
            collected_at: Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap(),
            windows: Vec::new(),
            detail: SnapshotDetail {
                reset_credits: Some(ResetCreditsDetail {
                    available_count: 4,
                    ..ResetCreditsDetail::default()
                }),
                ..SnapshotDetail::default()
            },
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
            detail: SnapshotDetail {
                activity: Some(ActivityDetail {
                    source: Some("provider_reported".to_string()),
                    lifetime_tokens: Some(1),
                    by_day: vec![day("2026-07-09", 1, 0, 0, None)],
                    ..ActivityDetail::default()
                }),
                ..SnapshotDetail::default()
            },
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
            detail: SnapshotDetail {
                activity: Some(ActivityDetail {
                    source: Some("future_stream_v9".to_string()),
                    by_day: vec![day("2026-07-11", 42, 0, 0, None)],
                    ..ActivityDetail::default()
                }),
                dataset_provenance: vec![DatasetProvenance {
                    source_id: String::new(),
                    authoritative: false,
                    provenance: DataProvenance {
                        source: UsageDataSource::LocalDatabase,
                        scope: UsageDataScope::Workspace,
                        quality: UsageDataQuality::Observed,
                        completeness: UsageDataCompleteness::Complete,
                        confidence: UsageDataConfidence::Medium,
                    },
                    window_ids: Vec::new(),
                    daily_sources: vec!["future_stream_v9".to_string()],
                    metadata_keys: Vec::new(),
                }],
                ..SnapshotDetail::default()
            },
        };

        let dashboard = build_usage_dashboard(&[snapshot], &[]);
        let provenance = &dashboard.accounts[0].activity.as_ref().unwrap().provenance;

        assert_eq!(provenance.source, UsageDataSource::LocalDatabase);
        assert_eq!(provenance.scope, UsageDataScope::Workspace);
        assert_eq!(provenance.completeness, UsageDataCompleteness::Complete);
    }
}
