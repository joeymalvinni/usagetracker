use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use usage_core::{
    ProviderId, UsageAmount, UsageDataCompleteness, UsageDataQuality, UsageDataScope,
    UsageDataSource, UsageUnit, UsageWindow, UsageWindowKind,
};

use crate::providers::{
    DailyUsageBucket, ProviderCollectionResult, ProviderError, ProviderErrorKind, ProviderUsage,
    UsageDataset,
};

use super::{auth::SessionSource, client::CursorFetch, number::NumberLike, CURSOR_PROVIDER_ID};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CursorUsageSummary {
    #[serde(default)]
    billing_cycle_start: Option<String>,
    #[serde(default)]
    billing_cycle_end: Option<String>,
    #[serde(default)]
    membership_type: Option<String>,
    #[serde(default)]
    limit_type: Option<String>,
    #[serde(default)]
    is_unlimited: Option<bool>,
    #[serde(default)]
    individual_usage: Option<CursorIndividualUsage>,
    #[serde(default)]
    team_usage: Option<CursorTeamUsage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorIndividualUsage {
    #[serde(default)]
    plan: Option<CursorPlanUsage>,
    #[serde(default)]
    on_demand: Option<CursorAmountUsage>,
    #[serde(default)]
    overall: Option<CursorAmountUsage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorPlanUsage {
    #[serde(default)]
    used: Option<NumberLike>,
    #[serde(default)]
    limit: Option<NumberLike>,
    #[serde(default)]
    remaining: Option<NumberLike>,
    #[serde(default)]
    auto_percent_used: Option<NumberLike>,
    #[serde(default)]
    api_percent_used: Option<NumberLike>,
    #[serde(default)]
    total_percent_used: Option<NumberLike>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CursorAmountUsage {
    #[serde(default)]
    used: Option<NumberLike>,
    #[serde(default)]
    limit: Option<NumberLike>,
    #[serde(default)]
    remaining: Option<NumberLike>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CursorTeamUsage {
    #[serde(default, rename = "onDemand")]
    on_demand: Option<CursorAmountUsage>,
    #[serde(default)]
    pooled: Option<CursorAmountUsage>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CursorUserInfo {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    sub: Option<String>,
}

impl CursorUserInfo {
    pub(super) fn stable_id(&self) -> Option<&str> {
        self.sub
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
    }

    pub(super) fn email(&self) -> Option<&str> {
        self.email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
    }

    pub(super) fn display_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }
}

impl CursorUsageSummary {
    pub(super) fn billing_period(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let start = parse_time(self.billing_cycle_start.as_deref())?;
        let end = parse_time(self.billing_cycle_end.as_deref())?.min(Utc::now());
        (start <= end).then_some((start, end))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct CursorUsageResponse {
    #[serde(default, rename = "gpt-4")]
    gpt4: Option<CursorModelUsage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorModelUsage {
    #[serde(default)]
    num_requests: Option<NumberLike>,
    #[serde(default)]
    num_requests_total: Option<NumberLike>,
    #[serde(default)]
    max_request_usage: Option<NumberLike>,
}

pub(super) struct NormalizedCursorUsage {
    pub(super) collection: ProviderCollectionResult,
    pub(super) scope: UsageDataScope,
    pub(super) supplemental: Vec<UsageDataset>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeadlineSource {
    PlanPercent,
    PlanRatio,
    Overall,
    Pooled,
}

impl HeadlineSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::PlanPercent => "plan",
            Self::PlanRatio => "plan_ratio",
            Self::Overall => "overall",
            Self::Pooled => "pooled",
        }
    }

    fn is_organization(self) -> bool {
        self == Self::Pooled
    }
}

pub(super) fn normalize_cursor_fetch(
    fetch: CursorFetch,
    source: SessionSource,
    account_id_fallback: &str,
) -> Result<NormalizedCursorUsage, ProviderError> {
    let CursorFetch {
        summary,
        identity,
        legacy,
        event_pages,
        event_warning,
    } = fetch;
    let reset_at = parse_time(summary.billing_cycle_end.as_deref());
    let plan = summary
        .individual_usage
        .as_ref()
        .and_then(|usage| usage.plan.as_ref());
    let overall = summary
        .individual_usage
        .as_ref()
        .and_then(|usage| usage.overall.as_ref());
    let pooled = summary
        .team_usage
        .as_ref()
        .and_then(|usage| usage.pooled.as_ref());
    let auto_percent = plan
        .and_then(|usage| usage.auto_percent_used.as_ref())
        .and_then(NumberLike::value)
        .map(clamp_percent);
    let api_percent = plan
        .and_then(|usage| usage.api_percent_used.as_ref())
        .and_then(NumberLike::value)
        .map(clamp_percent);
    let legacy_requests = legacy_request_usage(legacy.as_ref());
    let (headline_percent, headline_source) =
        headline(plan, overall, pooled, auto_percent, api_percent);

    let mut windows = Vec::new();
    let mut scope = UsageDataScope::AccountWide;
    if let Some((used, limit)) = legacy_requests {
        windows.push(amount_window(
            "cursor_total",
            "Cursor requests",
            UsageUnit::Requests,
            used,
            Some(limit),
            None,
            reset_at,
        ));
    } else {
        if let (Some(percent), Some(headline_source)) = (headline_percent, headline_source) {
            let label = if headline_source.is_organization() {
                scope = UsageDataScope::Organization;
                "Cursor team pool"
            } else {
                "Cursor total"
            };
            let window = match headline_source {
                HeadlineSource::PlanPercent | HeadlineSource::PlanRatio => {
                    percent_and_cents_window("cursor_total", label, percent, plan, reset_at)
                }
                HeadlineSource::Overall => {
                    percent_and_cents_window("cursor_total", label, percent, overall, reset_at)
                }
                HeadlineSource::Pooled => {
                    percent_and_cents_window("cursor_total", label, percent, pooled, reset_at)
                }
            };
            windows.push(window);
        }
        if let Some(percent) = auto_percent {
            windows.push(percent_window(
                "cursor_auto",
                "Cursor Auto",
                percent,
                reset_at,
            ));
        }
        if let Some(percent) = api_percent {
            windows.push(percent_window(
                "cursor_api",
                "Cursor API",
                percent,
                reset_at,
            ));
        }
    }

    let personal_on_demand = summary
        .individual_usage
        .as_ref()
        .and_then(|usage| usage.on_demand.as_ref())
        .filter(|usage| amount_is_visible(usage));
    let team_on_demand = summary
        .team_usage
        .as_ref()
        .and_then(|usage| usage.on_demand.as_ref())
        .filter(|usage| amount_is_visible(usage));
    let personal_windows = if scope == UsageDataScope::Organization {
        personal_on_demand
            .map(|usage| {
                vec![cents_window(
                    "cursor_on_demand",
                    "Cursor on-demand",
                    usage,
                    reset_at,
                )]
            })
            .unwrap_or_default()
    } else {
        if let Some(usage) = personal_on_demand {
            windows.push(cents_window(
                "cursor_on_demand",
                "Cursor on-demand",
                usage,
                reset_at,
            ));
        }
        Vec::new()
    };

    let account_email = identity
        .as_ref()
        .and_then(CursorUserInfo::email)
        .map(str::to_string);
    let account_id = identity
        .as_ref()
        .and_then(CursorUserInfo::stable_id)
        .unwrap_or(account_id_fallback);
    let headline_source_label = if legacy_requests.is_some() {
        Some("legacy_requests")
    } else {
        headline_source.map(HeadlineSource::as_str)
    };
    let metadata = json!({
        "collection_mode": "cursor_web",
        "credential_source": source.as_str(),
        "membership_type": summary.membership_type.clone(),
        "limit_type": summary.limit_type.clone(),
        "is_unlimited": summary.is_unlimited,
        "billing_cycle_start": summary.billing_cycle_start.clone(),
        "billing_cycle_end": summary.billing_cycle_end.clone(),
        "headline_source": headline_source_label,
        "account_external_id": account_id,
        "web_authoritative": true,
        "team_pooled_scope": legacy_requests.is_none()
            && headline_source.is_some_and(HeadlineSource::is_organization),
    });

    if windows.is_empty() && team_on_demand.is_none() {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Cursor usage summary did not contain a supported quota or spending window",
        ));
    }

    let mut supplemental = Vec::new();
    let mut primary_collection = collection(windows, metadata, account_email.clone());
    let mut organization_personal_collection = (scope == UsageDataScope::Organization)
        .then(|| collection(personal_windows, json!({}), account_email.clone()));
    if let Some(warning) = event_warning {
        primary_collection.warnings.push(warning);
    }
    if let Some(event_pages) = event_pages {
        let (period_start, period_end) = summary.billing_period().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Cursor usage event history did not have a valid billing period",
            )
        })?;
        match super::events::normalize_usage_events(event_pages, period_start, period_end) {
            Ok(report) => {
                if let Some(personal_collection) = organization_personal_collection.as_mut() {
                    personal_collection.daily_usage = report.daily_usage;
                    personal_collection.usage_events = Some(report.batch);
                    personal_collection.usage.metadata["cursor_cost"] = report.metadata;
                } else {
                    primary_collection.daily_usage = report.daily_usage;
                    primary_collection.usage_events = Some(report.batch);
                    primary_collection.usage.metadata["cursor_cost"] = report.metadata;
                }
            }
            Err(error) => primary_collection.warnings.push(format!(
                "Cursor usage event history was unavailable: {}",
                error.short_message()
            )),
        }
    }
    if let Some(personal_collection) = organization_personal_collection {
        if !personal_collection.usage.windows.is_empty()
            || personal_collection.usage_events.is_some()
        {
            supplemental.push(UsageDataset::authoritative_named_scoped(
                "cursor_personal_usage",
                personal_collection,
                UsageDataScope::AccountWide,
            ));
        }
    }
    if let Some(team_on_demand) = team_on_demand {
        if personal_on_demand.is_none() {
            let team_window = cents_window(
                "cursor_team_on_demand",
                "Cursor team on-demand",
                team_on_demand,
                reset_at,
            );
            if primary_collection.usage.windows.is_empty()
                && primary_collection.usage_events.is_none()
            {
                scope = UsageDataScope::Organization;
                return Ok(NormalizedCursorUsage {
                    collection: collection(
                        vec![team_window],
                        json!({
                            "collection_mode": "cursor_web",
                            "credential_source": source.as_str(),
                            "membership_type": summary.membership_type,
                            "limit_type": summary.limit_type,
                            "billing_cycle_end": summary.billing_cycle_end,
                            "team_budget_scope": true,
                            "web_authoritative": true,
                        }),
                        account_email,
                    ),
                    scope,
                    supplemental,
                });
            }
            supplemental.push(UsageDataset::supplemental_named(
                "cursor_team_budget",
                collection(
                    vec![team_window],
                    json!({
                        "cursor_team_budget": true,
                        "team_budget_scope": true,
                    }),
                    account_email,
                ),
                UsageDataSource::ProviderReported,
                UsageDataScope::Organization,
                UsageDataQuality::Authoritative,
                UsageDataCompleteness::Complete,
            ));
        }
    }
    Ok(NormalizedCursorUsage {
        collection: primary_collection,
        scope,
        supplemental,
    })
}

fn collection(
    windows: Vec<UsageWindow>,
    metadata: serde_json::Value,
    account_email: Option<String>,
) -> ProviderCollectionResult {
    ProviderCollectionResult {
        usage: ProviderUsage {
            provider_id: ProviderId::new(CURSOR_PROVIDER_ID),
            collected_at: Utc::now(),
            windows,
            metadata,
        },
        daily_usage: Vec::<DailyUsageBucket>::new(),
        usage_events: None,
        collection_mode: "cursor_web".to_string(),
        account_email,
        warnings: Vec::new(),
    }
}

fn headline(
    plan: Option<&CursorPlanUsage>,
    overall: Option<&CursorAmountUsage>,
    pooled: Option<&CursorAmountUsage>,
    auto_percent: Option<f64>,
    api_percent: Option<f64>,
) -> (Option<f64>, Option<HeadlineSource>) {
    if let Some(percent) = plan
        .and_then(|usage| usage.total_percent_used.as_ref())
        .and_then(NumberLike::value)
    {
        return (
            Some(clamp_percent(percent)),
            Some(HeadlineSource::PlanPercent),
        );
    }
    if let (Some(auto), Some(api)) = (auto_percent, api_percent) {
        return (
            Some(clamp_percent((auto + api) / 2.0)),
            Some(HeadlineSource::PlanPercent),
        );
    }
    if let Some(percent) = api_percent.or(auto_percent) {
        return (Some(percent), Some(HeadlineSource::PlanPercent));
    }
    if let Some(percent) = amount_percent(plan) {
        return (Some(percent), Some(HeadlineSource::PlanRatio));
    }
    if let Some(percent) = amount_percent(overall) {
        return (Some(percent), Some(HeadlineSource::Overall));
    }
    if let Some(percent) = amount_percent(pooled) {
        return (Some(percent), Some(HeadlineSource::Pooled));
    }
    (None, None)
}

fn legacy_request_usage(value: Option<&CursorUsageResponse>) -> Option<(f64, f64)> {
    let usage = value?.gpt4.as_ref()?;
    let used = usage
        .num_requests_total
        .as_ref()
        .or(usage.num_requests.as_ref())?
        .value()?;
    let limit = usage.max_request_usage.as_ref()?.value()?;
    (used >= 0.0 && limit > 0.0).then_some((used, limit))
}

fn amount_percent<T: AmountUsage>(usage: Option<&T>) -> Option<f64> {
    let used = usage?.used()?;
    let limit = usage?.limit()?;
    (used >= 0.0 && limit > 0.0).then(|| clamp_percent(used / limit * 100.0))
}

trait AmountUsage {
    fn used(&self) -> Option<f64>;
    fn limit(&self) -> Option<f64>;
    fn remaining(&self) -> Option<f64>;
}

impl AmountUsage for CursorPlanUsage {
    fn used(&self) -> Option<f64> {
        self.used.as_ref().and_then(NumberLike::value)
    }

    fn limit(&self) -> Option<f64> {
        self.limit.as_ref().and_then(NumberLike::value)
    }

    fn remaining(&self) -> Option<f64> {
        self.remaining.as_ref().and_then(NumberLike::value)
    }
}

impl AmountUsage for CursorAmountUsage {
    fn used(&self) -> Option<f64> {
        self.used.as_ref().and_then(NumberLike::value)
    }

    fn limit(&self) -> Option<f64> {
        self.limit.as_ref().and_then(NumberLike::value)
    }

    fn remaining(&self) -> Option<f64> {
        self.remaining.as_ref().and_then(NumberLike::value)
    }
}

fn percent_and_cents_window<T: AmountUsage>(
    id: &str,
    label: &str,
    percent: f64,
    amount: Option<&T>,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let used = amount
        .and_then(AmountUsage::used)
        .filter(|value| *value >= 0.0);
    let limit = amount
        .and_then(AmountUsage::limit)
        .filter(|value| *value >= 0.0);
    let remaining = amount
        .and_then(AmountUsage::remaining)
        .filter(|value| *value >= 0.0)
        .or_else(|| used.zip(limit).map(|(used, limit)| (limit - used).max(0.0)));
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Monthly,
        used: used.map(|value| usd(value / 100.0)),
        limit: limit.map(|value| usd(value / 100.0)),
        remaining: remaining.map(|value| usd(value / 100.0)),
        percent_used: Some(percent),
        percent_remaining: Some((100.0 - percent).clamp(0.0, 100.0)),
        reset_at,
    }
}

fn percent_window(
    id: &str,
    label: &str,
    percent: f64,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Monthly,
        used: None,
        limit: None,
        remaining: None,
        percent_used: Some(percent),
        percent_remaining: Some((100.0 - percent).clamp(0.0, 100.0)),
        reset_at,
    }
}

fn cents_window(
    id: &str,
    label: &str,
    usage: &CursorAmountUsage,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let used = usage
        .used
        .as_ref()
        .and_then(NumberLike::value)
        .filter(|value| *value >= 0.0)
        .unwrap_or(0.0);
    let limit = usage
        .limit
        .as_ref()
        .and_then(NumberLike::value)
        .filter(|value| *value >= 0.0);
    let remaining = usage
        .remaining
        .as_ref()
        .and_then(NumberLike::value)
        .filter(|value| *value >= 0.0)
        .or_else(|| limit.map(|limit| (limit - used).max(0.0)));
    let percent = limit
        .filter(|limit| *limit > 0.0)
        .map(|limit| clamp_percent(used / limit * 100.0));
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Monthly,
        used: Some(usd(used / 100.0)),
        limit: limit.map(|value| usd(value / 100.0)),
        remaining: remaining.map(|value| usd(value / 100.0)),
        percent_used: percent,
        percent_remaining: percent.map(|value| (100.0 - value).clamp(0.0, 100.0)),
        reset_at,
    }
}

fn amount_window(
    id: &str,
    label: &str,
    unit: UsageUnit,
    used: f64,
    limit: Option<f64>,
    remaining: Option<f64>,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let percent = limit
        .filter(|limit| *limit > 0.0)
        .map(|limit| clamp_percent(used / limit * 100.0));
    UsageWindow {
        window_id: id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Monthly,
        used: Some(UsageAmount {
            value: used,
            unit: unit.clone(),
        }),
        limit: limit.map(|value| UsageAmount {
            value,
            unit: unit.clone(),
        }),
        remaining: remaining
            .or_else(|| limit.map(|limit| (limit - used).max(0.0)))
            .map(|value| UsageAmount { value, unit }),
        percent_used: percent,
        percent_remaining: percent.map(|value| (100.0 - value).clamp(0.0, 100.0)),
        reset_at,
    }
}

fn amount_is_visible(usage: &CursorAmountUsage) -> bool {
    usage
        .used
        .as_ref()
        .and_then(NumberLike::value)
        .is_some_and(|value| value > 0.0)
        || usage
            .limit
            .as_ref()
            .and_then(NumberLike::value)
            .is_some_and(|value| value > 0.0)
}

fn usd(value: f64) -> UsageAmount {
    UsageAmount {
        value,
        unit: UsageUnit::Usd,
    }
}

fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}
