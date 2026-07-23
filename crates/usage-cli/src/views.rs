use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Days, Local, NaiveDate, TimeDelta, Utc};
use serde::Serialize;
use usage_core::{
    AggregateProvenance, PricingCoverage, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind,
};

use crate::selection::{latest_snapshots, SelectedState};

#[derive(Debug, Serialize)]
pub struct SummaryView {
    #[serde(rename = "type")]
    response_type: &'static str,
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<SummaryProvider>,
}

#[derive(Debug, Serialize)]
pub struct SummaryProvider {
    pub provider_id: String,
    pub display_name: String,
    pub enabled: bool,
    pub data_state: DataState,
    pub account_count: usize,
    pub limits: Vec<SummaryLimit>,
    pub today_tokens: Option<u64>,
    pub lookback_tokens: Option<u64>,
    pub lookback_cost_usd: Option<f64>,
    pub oldest_snapshot_at: Option<DateTime<Utc>>,
    pub stale: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataState {
    Available,
    NoData,
    Disabled,
}

#[derive(Debug, Serialize)]
pub struct SummaryLimit {
    pub role: LimitRole,
    pub label: String,
    pub minimum_percent_remaining: Option<f64>,
    pub maximum_percent_remaining: Option<f64>,
    pub minimum_remaining: Option<f64>,
    pub maximum_remaining: Option<f64>,
    pub unit: Option<String>,
    pub next_reset_at: Option<DateTime<Utc>>,
    pub account_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitRole {
    Session,
    Weekly,
    Monthly,
    Credits,
    Other,
}

#[derive(Debug)]
struct LimitAccumulator {
    role: LimitRole,
    label: String,
    percentages: Vec<f64>,
    remaining: Vec<f64>,
    unit: Option<String>,
    resets: Vec<DateTime<Utc>>,
    accounts: BTreeSet<String>,
}

impl SummaryView {
    pub fn build(selected: &SelectedState, now: DateTime<Utc>) -> Self {
        let latest = latest_snapshots(&selected.snapshots);
        let stale_seconds = selected
            .config
            .poll_interval_seconds
            .saturating_mul(2)
            .max(30 * 60)
            .min(i64::MAX as u64) as i64;
        let stale_after = TimeDelta::seconds(stale_seconds);
        let providers = selected
            .provider_ids
            .iter()
            .map(|provider_id| {
                let info = selected.provider_info(provider_id);
                let snapshots = latest
                    .iter()
                    .filter_map(|((provider, _), snapshot)| {
                        (*provider == provider_id).then_some(*snapshot)
                    })
                    .collect::<Vec<_>>();
                let summaries = selected
                    .dashboard
                    .accounts
                    .iter()
                    .filter(|summary| summary.provider_id.as_str() == provider_id)
                    .collect::<Vec<_>>();
                let account_ids = selected
                    .accounts
                    .iter()
                    .filter(|account| account.provider_id.as_str() == provider_id)
                    .map(|account| account.id.as_str())
                    .chain(
                        snapshots
                            .iter()
                            .map(|snapshot| snapshot.account_id.as_str()),
                    )
                    .chain(summaries.iter().map(|summary| summary.account_id.as_str()))
                    .collect::<BTreeSet<_>>();
                let oldest_snapshot_at =
                    snapshots.iter().map(|snapshot| snapshot.collected_at).min();
                let stale = oldest_snapshot_at.is_some_and(|oldest| now - oldest > stale_after);
                let available = !snapshots.is_empty() || !summaries.is_empty();
                let activities = summaries
                    .iter()
                    .filter_map(|summary| summary.activity.as_ref())
                    .collect::<Vec<_>>();
                let snapshot_accounts = snapshots
                    .iter()
                    .map(|snapshot| snapshot.account_id.as_str())
                    .collect::<BTreeSet<_>>();
                SummaryProvider {
                    provider_id: provider_id.clone(),
                    display_name: info.display_name.clone(),
                    enabled: info.enabled,
                    data_state: if !info.enabled {
                        DataState::Disabled
                    } else if available {
                        DataState::Available
                    } else {
                        DataState::NoData
                    },
                    account_count: account_ids.len(),
                    limits: summarize_limits(&snapshots),
                    today_tokens: sum_optional_u64(
                        activities.iter().map(|activity| activity.today_tokens),
                    ),
                    lookback_tokens: sum_optional_u64(
                        activities.iter().map(|activity| activity.lookback_tokens),
                    ),
                    lookback_cost_usd: sum_optional(
                        summaries
                            .iter()
                            .filter_map(|summary| summary.cost.as_ref())
                            .filter(|cost| cost.days.iter().any(|day| day.cost_usd.is_some()))
                            .map(|cost| cost.lookback_cost_usd),
                    ),
                    oldest_snapshot_at,
                    stale: stale
                        || (!snapshots.is_empty()
                            && account_ids
                                .iter()
                                .any(|account_id| !snapshot_accounts.contains(account_id))),
                }
            })
            .collect();
        Self {
            response_type: "summary",
            generated_at: selected.generated_at,
            providers,
        }
    }
}

fn summarize_limits(snapshots: &[&usage_core::UsageSnapshot]) -> Vec<SummaryLimit> {
    let mut groups = BTreeMap::<(LimitRole, String, Option<String>), LimitAccumulator>::new();
    for snapshot in snapshots {
        for window in summary_windows(snapshot) {
            let role = limit_role(window);
            let label = limit_label(window, role);
            let absolute = absolute_remaining(window);
            let unit = absolute.as_ref().map(|amount| unit_name(&amount.unit));
            let group = groups
                .entry((role, label.clone(), unit.clone()))
                .or_insert_with(|| LimitAccumulator {
                    role,
                    label,
                    percentages: Vec::new(),
                    remaining: Vec::new(),
                    unit,
                    resets: Vec::new(),
                    accounts: BTreeSet::new(),
                });
            if let Some(percent) = percent_remaining(window) {
                group.percentages.push(percent);
            }
            if let Some(amount) = absolute {
                group.remaining.push(amount.value);
            }
            if let Some(reset) = window.reset_at {
                group.resets.push(reset);
            }
            group.accounts.insert(snapshot.account_id.to_string());
        }
    }
    groups
        .into_values()
        .map(|group| SummaryLimit {
            role: group.role,
            label: group.label,
            minimum_percent_remaining: minimum(&group.percentages),
            maximum_percent_remaining: maximum(&group.percentages),
            minimum_remaining: minimum(&group.remaining),
            maximum_remaining: maximum(&group.remaining),
            unit: group.unit,
            next_reset_at: group.resets.into_iter().min(),
            account_count: group.accounts.len(),
        })
        .collect()
}

fn summary_windows(snapshot: &usage_core::UsageSnapshot) -> Vec<&UsageWindow> {
    let mut selected = Vec::new();
    for role in [LimitRole::Session, LimitRole::Weekly, LimitRole::Monthly] {
        if let Some(window) = snapshot
            .windows
            .iter()
            .filter(|window| {
                !matches!(
                    window.kind,
                    UsageWindowKind::Tokens | UsageWindowKind::Credits
                )
            })
            .filter(|window| percent_remaining(window).is_some())
            .filter(|window| limit_role(window) == role)
            .min_by_key(|window| {
                let name = format!("{} {}", window.window_id, window.label).to_ascii_lowercase();
                usize::from(name.contains("additional") || name.contains("spark"))
            })
        {
            selected.push(window);
        }
    }
    selected.extend(snapshot.windows.iter().filter(|window| {
        limit_role(window) == LimitRole::Other
            && !matches!(window.kind, UsageWindowKind::Tokens)
            && percent_remaining(window).is_some()
    }));
    selected.extend(snapshot.windows.iter().filter(|window| {
        matches!(window.kind, UsageWindowKind::Credits) && absolute_remaining(window).is_some()
    }));
    selected
}

fn limit_role(window: &UsageWindow) -> LimitRole {
    match window.kind {
        UsageWindowKind::Session => LimitRole::Session,
        UsageWindowKind::Weekly => LimitRole::Weekly,
        UsageWindowKind::Monthly => LimitRole::Monthly,
        UsageWindowKind::Credits => LimitRole::Credits,
        _ => {
            let name = format!("{} {}", window.window_id, window.label).to_ascii_lowercase();
            if name.contains("session") || name.contains("five_hour") || name.contains("five hour")
            {
                LimitRole::Session
            } else if name.contains("weekly")
                || name.contains("seven_day")
                || name.contains("seven day")
            {
                LimitRole::Weekly
            } else if name.contains("monthly") || name.contains("30d") || name.contains("30 days") {
                LimitRole::Monthly
            } else {
                LimitRole::Other
            }
        }
    }
}

fn limit_label(window: &UsageWindow, role: LimitRole) -> String {
    match role {
        LimitRole::Session => {
            let name = format!("{} {}", window.window_id, window.label).to_ascii_lowercase();
            if name.contains("five_hour") || name.contains("five hour") || name.contains("5h") {
                "5h".to_string()
            } else {
                "session".to_string()
            }
        }
        LimitRole::Weekly => "week".to_string(),
        LimitRole::Monthly => "month".to_string(),
        LimitRole::Credits => "credits".to_string(),
        _ => window.label.clone(),
    }
}

fn percent_remaining(window: &UsageWindow) -> Option<f64> {
    window
        .percent_remaining
        .or_else(|| window.percent_used.map(|used| 100.0 - used))
        .map(|percent| percent.clamp(0.0, 100.0))
}

fn absolute_remaining(window: &UsageWindow) -> Option<&UsageAmount> {
    if matches!(window.kind, UsageWindowKind::Credits) || percent_remaining(window).is_none() {
        window.remaining.as_ref()
    } else {
        None
    }
}

fn unit_name(unit: &UsageUnit) -> String {
    match unit {
        UsageUnit::Tokens => "tokens",
        UsageUnit::Requests => "requests",
        UsageUnit::Credits => "credits",
        UsageUnit::Usd => "usd",
        UsageUnit::Percent => "percent",
        UsageUnit::Unknown => "unknown",
    }
    .to_string()
}

fn minimum(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::min)
}

fn maximum(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

fn sum_optional(values: impl Iterator<Item = f64>) -> Option<f64> {
    let values = values.collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.into_iter().sum())
}

fn sum_optional_u64(values: impl Iterator<Item = u64>) -> Option<u64> {
    let values = values.collect::<Vec<_>>();
    (!values.is_empty()).then(|| {
        values
            .into_iter()
            .fold(0_u64, |total, value| total.saturating_add(value))
    })
}

#[derive(Debug, Serialize)]
pub struct ActivityView {
    #[serde(rename = "type")]
    response_type: &'static str,
    pub range: ActivityRange,
    pub filters: ActivityFilters,
    pub days: Vec<ActivityDay>,
    pub pricing: PricingCoverage,
    pub provenance: AggregateProvenance,
    #[serde(skip)]
    pub title_providers: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActivityDay {
    pub date: NaiveDate,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct ActivityRange {
    pub days: u8,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub timezone: String,
}

#[derive(Debug, Serialize)]
pub struct ActivityFilters {
    pub providers: Vec<String>,
    pub accounts: Vec<String>,
}

impl ActivityView {
    pub fn build(selected: &SelectedState, requested_days: u8, today: NaiveDate) -> Self {
        let start_date = today
            .checked_sub_days(Days::new(u64::from(requested_days - 1)))
            .expect("30-day activity range must fit in NaiveDate");
        let by_date = selected
            .dashboard
            .days
            .iter()
            .map(|day| (day.date, day))
            .collect::<BTreeMap<_, _>>();
        let days = (0..requested_days)
            .map(|offset| {
                let date = start_date
                    .checked_add_days(Days::new(u64::from(offset)))
                    .expect("30-day activity range must fit in NaiveDate");
                by_date.get(&date).map_or_else(
                    || ActivityDay {
                        date,
                        tokens: 0,
                        cost_usd: None,
                        priced_tokens: 0,
                        unpriced_tokens: 0,
                    },
                    |day| ActivityDay {
                        date: day.date,
                        tokens: day.tokens,
                        cost_usd: day.cost_usd,
                        priced_tokens: day.priced_tokens,
                        unpriced_tokens: day.unpriced_tokens,
                    },
                )
            })
            .collect::<Vec<_>>();
        let priced_tokens = days.iter().map(|day| day.priced_tokens).sum();
        let unpriced_tokens = days.iter().map(|day| day.unpriced_tokens).sum();
        let pricing = PricingCoverage {
            priced_tokens,
            unpriced_tokens,
            covered_percent: if priced_tokens + unpriced_tokens == 0 {
                0.0
            } else {
                priced_tokens as f64 / (priced_tokens + unpriced_tokens) as f64 * 100.0
            },
            unpriced_models: if unpriced_tokens > 0 {
                selected.dashboard.pricing.unpriced_models.clone()
            } else {
                Vec::new()
            },
            catalog_version: selected.dashboard.pricing.catalog_version.clone(),
            catalog_source: selected.dashboard.pricing.catalog_source.clone(),
            catalog_effective_from: selected.dashboard.pricing.catalog_effective_from,
        };
        Self {
            response_type: "activity",
            range: ActivityRange {
                days: requested_days,
                start_date,
                end_date: today,
                timezone: local_timezone_name(),
            },
            filters: ActivityFilters {
                providers: selected.provider_ids.clone(),
                accounts: selected
                    .accounts
                    .iter()
                    .map(|account| account.id.to_string())
                    .collect(),
            },
            days,
            pricing,
            provenance: selected.dashboard.provenance.clone(),
            title_providers: selected
                .provider_ids
                .iter()
                .map(|provider_id| selected.provider_info(provider_id).display_name.clone())
                .collect(),
        }
    }

    pub fn today_local() -> NaiveDate {
        Local::now().date_naive()
    }
}

fn local_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| Local::now().offset().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::SelectionRequest;
    use usage_core::{
        aggregate_usage_dashboard, AccountId, AccountUsageSummary, ActivitySummary, ApiResponse,
        CostSummary, DailyUsagePoint, DataProvenance, ProviderId, ResponseEnvelope, StateSnapshot,
        UsageDataCompleteness, UsageDataConfidence, UsageDataQuality, UsageDataScope,
        UsageDataSource, UsageSnapshot,
    };

    fn fixture_state() -> StateSnapshot {
        let envelope: ResponseEnvelope =
            serde_json::from_str(include_str!("../../usage-core/wire-fixtures/state_v3.json"))
                .unwrap();
        let ApiResponse::State { state } = envelope.response else {
            panic!("expected state fixture");
        };
        state
    }

    fn provenance() -> DataProvenance {
        DataProvenance {
            source: UsageDataSource::LocalLogs,
            scope: UsageDataScope::ThisDevice,
            quality: UsageDataQuality::Estimated,
            completeness: UsageDataCompleteness::Partial,
            confidence: UsageDataConfidence::Medium,
        }
    }

    #[test]
    fn activity_zero_fills_range_and_recomputes_range_pricing() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();
        let active_date = today.checked_sub_days(Days::new(1)).unwrap();
        let activity_day = DailyUsagePoint {
            date: active_date,
            tokens: 100,
            cost_usd: None,
            priced_tokens: 0,
            unpriced_tokens: 0,
        };
        let cost_day = DailyUsagePoint {
            date: active_date,
            tokens: 100,
            cost_usd: Some(0.5),
            priced_tokens: 75,
            unpriced_tokens: 25,
        };
        let mut state = fixture_state();
        state.dashboard = aggregate_usage_dashboard(vec![AccountUsageSummary {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("default"),
            activity: Some(ActivitySummary {
                provenance: provenance(),
                days: vec![activity_day],
                today_tokens: 0,
                lookback_tokens: 100,
                lifetime_tokens: Some(100),
            }),
            cost: Some(CostSummary {
                provenance: provenance(),
                days: vec![cost_day],
                today_cost_usd: 0.0,
                lookback_cost_usd: 0.5,
                pricing: PricingCoverage {
                    priced_tokens: 75,
                    unpriced_tokens: 25,
                    covered_percent: 75.0,
                    unpriced_models: vec!["future-model".to_string()],
                    catalog_version: None,
                    catalog_source: None,
                    catalog_effective_from: None,
                },
                models: Vec::new(),
            }),
            reset_credits: None,
        }]);
        let selected = SelectedState::from_state(state, SelectionRequest::default()).unwrap();

        let view = ActivityView::build(&selected, 3, today);

        assert_eq!(view.days.len(), 3);
        assert_eq!(view.days[0].tokens, 0);
        assert_eq!(view.days[1].tokens, 100);
        assert_eq!(view.days[1].cost_usd, Some(0.5));
        assert_eq!(view.days[2].cost_usd, None);
        assert_eq!(view.pricing.priced_tokens, 75);
        assert_eq!(view.pricing.unpriced_tokens, 25);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["days"][0]["cost_usd"].is_null());
    }

    #[test]
    fn activity_dashboard_omits_aggregate_provenance_explanation() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();
        let state = fixture_state();
        let mut selected = SelectedState::from_state(state, SelectionRequest::default()).unwrap();
        selected.dashboard.provenance.mixed_scope = true;
        selected.dashboard.provenance.explanation = "aggregate provenance warning".to_string();
        let view = ActivityView::build(&selected, 3, today);

        let rendered = crate::render::render_activity(&view, false);

        assert!(!rendered.contains("aggregate provenance warning"));
    }

    #[test]
    fn summary_uses_ranges_instead_of_averaging_account_limits() {
        let now = Utc::now();
        let mut state = fixture_state();
        state.snapshots = [18.0, 82.0]
            .into_iter()
            .enumerate()
            .map(|(index, remaining)| UsageSnapshot {
                provider_id: ProviderId::new("codex"),
                account_id: AccountId::new(format!("account-{index}")),
                collected_at: now,
                windows: vec![UsageWindow {
                    window_id: "session".to_string(),
                    label: "5h".to_string(),
                    kind: UsageWindowKind::Session,
                    used: None,
                    limit: None,
                    remaining: None,
                    percent_used: None,
                    percent_remaining: Some(remaining),
                    reset_at: None,
                }],
                metadata: serde_json::Value::Null,
            })
            .collect();
        let selected = SelectedState::from_state(state, SelectionRequest::default()).unwrap();

        let view = SummaryView::build(&selected, now);
        let limit = &view.providers[0].limits[0];

        assert_eq!(view.providers[0].account_count, 2);
        assert_eq!(limit.minimum_percent_remaining, Some(18.0));
        assert_eq!(limit.maximum_percent_remaining, Some(82.0));
        assert_eq!(limit.account_count, 2);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["providers"][0]["today_tokens"].is_null());
    }
}
