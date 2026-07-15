use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AccountId, ProviderId};

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountDisplayNameSource {
    Provider,
    #[default]
    Generated,
    User,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct Account {
    pub id: AccountId,
    pub provider_id: ProviderId,
    pub external_account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub display_name: Option<String>,
    #[serde(default)]
    pub display_name_source: AccountDisplayNameSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default = "default_collection_enabled")]
    pub collection_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_collection_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct UsageSnapshot {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub collected_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    #[serde(
        default,
        rename = "diagnostics",
        skip_serializing_if = "diagnostics_are_empty"
    )]
    pub metadata: serde_json::Value,
}

fn diagnostics_are_empty(value: &serde_json::Value) -> bool {
    value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty)
}

impl UsageSnapshot {
    fn dataset_provenance(&self) -> Vec<DatasetProvenance> {
        self.metadata
            .get("dataset_provenance")
            .and_then(|value| Vec::<DatasetProvenance>::deserialize(value).ok())
            .unwrap_or_default()
    }

    /// Describes whether a quota-like window can safely drive forecasts and
    /// alerts. Provider collectors still own parsing; this compatibility
    /// adapter makes the normalized semantic explicit at the API boundary.
    pub fn window_provenance(&self, window: &UsageWindow) -> UsageWindowProvenance {
        let datasets = self.dataset_provenance();
        self.window_provenance_from(&datasets, window)
    }

    /// Resolves every window while parsing the persisted dataset mapping once.
    pub fn windows_provenance(&self) -> Vec<UsageWindowProvenance> {
        let datasets = self.dataset_provenance();
        if datasets.len() <= 4 {
            return self
                .windows
                .iter()
                .map(|window| self.window_provenance_from(&datasets, window))
                .collect();
        }

        let indexed_window_count = datasets
            .iter()
            .map(|dataset| dataset.window_ids.len())
            .sum();
        let mut datasets_by_window = HashMap::with_capacity(indexed_window_count);
        for dataset in &datasets {
            for window_id in &dataset.window_ids {
                // Preserve the former linear search's first-match behavior for
                // malformed metadata that assigns a window to two datasets.
                datasets_by_window
                    .entry(window_id.as_str())
                    .or_insert(dataset);
            }
        }
        self.windows
            .iter()
            .map(|window| {
                self.window_provenance_for(
                    datasets_by_window.get(window.window_id.as_str()).copied(),
                    window,
                )
            })
            .collect()
    }

    fn window_provenance_from(
        &self,
        datasets: &[DatasetProvenance],
        window: &UsageWindow,
    ) -> UsageWindowProvenance {
        let dataset = datasets
            .iter()
            .find(|dataset| dataset.window_ids.iter().any(|id| id == &window.window_id));
        self.window_provenance_for(dataset, window)
    }

    fn window_provenance_for(
        &self,
        dataset: Option<&DatasetProvenance>,
        window: &UsageWindow,
    ) -> UsageWindowProvenance {
        if let Some(dataset) = dataset {
            let quota_like = (window.percent_used.is_some() || window.percent_remaining.is_some())
                && !matches!(
                    window.kind,
                    UsageWindowKind::Credits | UsageWindowKind::Tokens
                );
            return UsageWindowProvenance {
                provider_id: self.provider_id.clone(),
                account_id: self.account_id.clone(),
                window_id: window.window_id.clone(),
                source: dataset.provenance.source,
                scope: dataset.provenance.scope,
                quality: dataset.provenance.quality,
                completeness: dataset.provenance.completeness,
                confidence: dataset.provenance.confidence,
                authoritative: dataset.authoritative,
                quota_like,
            };
        }
        let synthetic_local_window = self
            .metadata
            .get("web_authoritative")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
            && self
                .metadata
                .get("estimate")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        let quota_like = (window.percent_used.is_some() || window.percent_remaining.is_some())
            && !matches!(
                window.kind,
                UsageWindowKind::Credits | UsageWindowKind::Tokens
            );

        if synthetic_local_window {
            UsageWindowProvenance {
                provider_id: self.provider_id.clone(),
                account_id: self.account_id.clone(),
                window_id: window.window_id.clone(),
                source: UsageDataSource::SyntheticLocalEstimate,
                scope: UsageDataScope::ThisDevice,
                quality: UsageDataQuality::Estimated,
                completeness: UsageDataCompleteness::Partial,
                confidence: UsageDataConfidence::Low,
                authoritative: false,
                quota_like,
            }
        } else {
            UsageWindowProvenance {
                provider_id: self.provider_id.clone(),
                account_id: self.account_id.clone(),
                window_id: window.window_id.clone(),
                source: UsageDataSource::ProviderReported,
                scope: UsageDataScope::AccountWide,
                quality: UsageDataQuality::Authoritative,
                completeness: UsageDataCompleteness::Complete,
                confidence: UsageDataConfidence::High,
                authoritative: true,
                quota_like,
            }
        }
    }

    pub fn window_is_authoritative_quota(&self, window: &UsageWindow) -> bool {
        let provenance = self.window_provenance(window);
        provenance.quota_like && provenance.authoritative
    }

    /// Resolves the typed origin of a persisted daily-usage source. This keeps
    /// shared dashboard code independent of provider-specific source labels.
    pub fn daily_provenance(&self, source: &str) -> Option<DataProvenance> {
        self.dataset_provenance()
            .into_iter()
            .find(|dataset| {
                dataset
                    .daily_sources
                    .iter()
                    .any(|candidate| candidate == source)
            })
            .map(|dataset| dataset.provenance)
    }
}

/// Persisted mapping from normalized data to its typed origin. It lives in the
/// diagnostics object for wire compatibility, but consumers deserialize this
/// type instead of inferring correctness from provider-specific strings.
#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct DatasetProvenance {
    pub authoritative: bool,
    pub provenance: DataProvenance,
    #[serde(default)]
    pub window_ids: Vec<String>,
    #[serde(default)]
    pub daily_sources: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDataSource {
    ProviderReported,
    LocalLogs,
    LocalDatabase,
    SyntheticLocalEstimate,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDataScope {
    AccountWide,
    ThisDevice,
    SelectedLocalRoots,
    Workspace,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDataQuality {
    Authoritative,
    Observed,
    Estimated,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDataCompleteness {
    Complete,
    Partial,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDataConfidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct UsageWindowProvenance {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub window_id: String,
    pub source: UsageDataSource,
    pub scope: UsageDataScope,
    pub quality: UsageDataQuality,
    pub completeness: UsageDataCompleteness,
    pub confidence: UsageDataConfidence,
    pub authoritative: bool,
    pub quota_like: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct DailyUsagePoint {
    pub date: NaiveDate,
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub priced_tokens: u64,
    #[serde(default)]
    pub unpriced_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActivitySummary {
    pub provenance: DataProvenance,
    pub days: Vec<DailyUsagePoint>,
    pub today_tokens: u64,
    pub lookback_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CostSummary {
    pub provenance: DataProvenance,
    pub days: Vec<DailyUsagePoint>,
    pub today_cost_usd: f64,
    pub lookback_cost_usd: f64,
    pub pricing: PricingCoverage,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct PricingCoverage {
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
    pub covered_percent: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_effective_from: Option<NaiveDate>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct DataProvenance {
    pub source: UsageDataSource,
    pub scope: UsageDataScope,
    pub quality: UsageDataQuality,
    pub completeness: UsageDataCompleteness,
    pub confidence: UsageDataConfidence,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AccountUsageSummary {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivitySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCreditSummary>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct ResetCreditSummary {
    pub available_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_expires_at: Option<DateTime<Utc>>,
    pub credits: Vec<ResetCredit>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct ResetCredit {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct AggregateProvenance {
    pub scopes: Vec<UsageDataScope>,
    pub qualities: Vec<UsageDataQuality>,
    pub partial: bool,
    pub estimated: bool,
    pub mixed_scope: bool,
    pub explanation: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct UsageDashboardSummary {
    pub accounts: Vec<AccountUsageSummary>,
    pub days: Vec<DailyUsagePoint>,
    pub pricing: PricingCoverage,
    pub provenance: AggregateProvenance,
}

/// Rebuilds every dashboard aggregate from the selected account summaries.
/// Keeping this in the shared model crate lets daemon and clients apply a
/// filter without allowing the typed totals and provenance to drift.
pub fn aggregate_usage_dashboard(accounts: Vec<AccountUsageSummary>) -> UsageDashboardSummary {
    let mut days = BTreeMap::<NaiveDate, DailyUsagePoint>::new();
    let mut scopes = Vec::new();
    let mut qualities = Vec::new();
    let mut partial = false;
    let mut estimated = false;
    let mut unpriced_models = BTreeSet::new();
    let mut catalog_sources = BTreeSet::new();
    let mut catalog_versions = BTreeSet::new();
    let mut catalog_effective_dates = BTreeSet::new();

    for summary in &accounts {
        if let Some(activity) = &summary.activity {
            push_unique(&mut scopes, activity.provenance.scope);
            push_unique(&mut qualities, activity.provenance.quality);
            partial |= activity.provenance.completeness == UsageDataCompleteness::Partial;
            estimated |= activity.provenance.quality == UsageDataQuality::Estimated;
            for point in &activity.days {
                let aggregate = days
                    .entry(point.date)
                    .or_insert_with(|| empty_dashboard_day(point.date));
                aggregate.tokens = aggregate.tokens.saturating_add(point.tokens);
            }
        }
        if let Some(cost) = &summary.cost {
            push_unique(&mut scopes, cost.provenance.scope);
            push_unique(&mut qualities, cost.provenance.quality);
            partial |= cost.provenance.completeness == UsageDataCompleteness::Partial;
            estimated |= cost.provenance.quality == UsageDataQuality::Estimated;
            unpriced_models.extend(cost.pricing.unpriced_models.iter().cloned());
            if let Some(source) = &cost.pricing.catalog_source {
                catalog_sources.insert(source.clone());
            }
            if let Some(version) = &cost.pricing.catalog_version {
                catalog_versions.insert(version.clone());
            }
            if let Some(effective_from) = cost.pricing.catalog_effective_from {
                catalog_effective_dates.insert(effective_from);
            }
            for point in &cost.days {
                let aggregate = days
                    .entry(point.date)
                    .or_insert_with(|| empty_dashboard_day(point.date));
                if let Some(cost_usd) = point.cost_usd {
                    aggregate.cost_usd = Some(aggregate.cost_usd.unwrap_or(0.0) + cost_usd);
                }
                aggregate.priced_tokens =
                    aggregate.priced_tokens.saturating_add(point.priced_tokens);
                aggregate.unpriced_tokens = aggregate
                    .unpriced_tokens
                    .saturating_add(point.unpriced_tokens);
                if summary.activity.is_none() {
                    aggregate.tokens = aggregate.tokens.saturating_add(point.tokens);
                }
            }
        }
    }

    let priced_tokens = days.values().fold(0_u64, |total, point| {
        total.saturating_add(point.priced_tokens)
    });
    let unpriced_tokens = days.values().fold(0_u64, |total, point| {
        total.saturating_add(point.unpriced_tokens)
    });
    let mixed_scope = scopes.len() > 1;
    let explanation = if mixed_scope {
        "Combined activity mixes account-wide provider data with this Mac or selected local logs; totals are not directly comparable billing records."
    } else if scopes.contains(&UsageDataScope::AccountWide) {
        "Activity is account-wide; cost remains an estimate unless explicitly reported as a provider bill."
    } else {
        "Activity reflects data observed on this Mac or in the configured local roots."
    }
    .to_string();

    UsageDashboardSummary {
        accounts,
        days: days.into_values().collect(),
        pricing: PricingCoverage {
            priced_tokens,
            unpriced_tokens,
            covered_percent: dashboard_covered_percent(priced_tokens, unpriced_tokens),
            unpriced_models: unpriced_models.into_iter().collect(),
            catalog_version: one_or_mixed(catalog_versions),
            catalog_source: one_or_mixed(catalog_sources),
            catalog_effective_from: one_or_none(catalog_effective_dates),
        },
        provenance: AggregateProvenance {
            scopes,
            qualities,
            partial,
            estimated,
            mixed_scope,
            explanation,
        },
    }
}

fn empty_dashboard_day(date: NaiveDate) -> DailyUsagePoint {
    DailyUsagePoint {
        date,
        tokens: 0,
        cost_usd: None,
        priced_tokens: 0,
        unpriced_tokens: 0,
    }
}

fn dashboard_covered_percent(priced: u64, unpriced: u64) -> f64 {
    let total = priced.saturating_add(unpriced);
    if total == 0 {
        0.0
    } else {
        (priced as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn push_unique<T: Copy + PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn one_or_mixed(values: BTreeSet<String>) -> Option<String> {
    match values.len() {
        0 => None,
        1 => values.into_iter().next(),
        _ => Some("mixed".to_string()),
    }
}

fn one_or_none<T: Ord>(mut values: BTreeSet<T>) -> Option<T> {
    (values.len() == 1).then(|| values.pop_first()).flatten()
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct UsageWindow {
    pub window_id: String,
    pub label: String,
    pub kind: UsageWindowKind,
    pub used: Option<UsageAmount>,
    pub limit: Option<UsageAmount>,
    pub remaining: Option<UsageAmount>,
    pub percent_used: Option<f64>,
    pub percent_remaining: Option<f64>,
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct UsageForecast {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub window_id: String,
    pub generated_at: DateTime<Utc>,
    pub reset_at: Option<DateTime<Utc>>,
    pub current_percent_used: f64,
    pub expected_percent_used: Option<f64>,
    pub pace_delta_percent: Option<f64>,
    pub rate_percent_per_hour: Option<f64>,
    pub projected_percent_at_reset: Option<f64>,
    #[serde(default)]
    pub projected_percent_remaining_at_reset: Option<f64>,
    pub predicted_exhaustion_at: Option<DateTime<Utc>>,
    pub status: ForecastStatus,
    pub sample_count: usize,
    pub confidence: ForecastConfidence,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastStatus {
    InsufficientData,
    Safe,
    OnPace,
    AtRisk,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastConfidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWindowKind {
    Session,
    Daily,
    Weekly,
    Monthly,
    Credits,
    Tokens,
    Other(String),
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct UsageAmount {
    pub value: f64,
    pub unit: UsageUnit,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageUnit {
    Tokens,
    Requests,
    Credits,
    Usd,
    Percent,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthStatus {
    Ok,
    CredentialsMissing,
    AuthFailed,
    KeychainAccessFailed,
    RateLimited,
    ProviderError,
    ParseError,
    BackingOff,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ProviderHealth {
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub status: ProviderHealthStatus,
    pub collection_mode: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn benchmark(name: &str, iterations: u32, mut operation: impl FnMut() -> usize) {
        for _ in 0..iterations.min(100) {
            std::hint::black_box(operation());
        }
        let started = std::time::Instant::now();
        let mut checksum = 0;
        for _ in 0..iterations {
            checksum ^= std::hint::black_box(operation());
        }
        let elapsed = started.elapsed();
        println!(
            "BENCH {name}: {:.2} us/iter ({iterations} iterations, checksum={checksum})",
            elapsed.as_secs_f64() * 1_000_000.0 / f64::from(iterations)
        );
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_core_response_pipeline() {
        let window_count = 256;
        let datasets = (0..64)
            .map(|dataset| {
                serde_json::json!({
                    "authoritative": dataset % 2 == 0,
                    "provenance": {
                        "source": "provider_reported",
                        "scope": "account_wide",
                        "quality": "authoritative",
                        "completeness": "complete",
                        "confidence": "high"
                    },
                    "window_ids": (0..4)
                        .map(|offset| format!("window-{}", dataset * 4 + offset))
                        .collect::<Vec<_>>(),
                    "daily_sources": [format!("source-{dataset}")]
                })
            })
            .collect::<Vec<_>>();
        let snapshot = UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("benchmark"),
            collected_at: Utc::now(),
            windows: (0..window_count)
                .map(|index| UsageWindow {
                    window_id: format!("window-{index}"),
                    label: format!("Window {index}"),
                    kind: UsageWindowKind::Weekly,
                    used: None,
                    limit: None,
                    remaining: None,
                    percent_used: Some(50.0),
                    percent_remaining: Some(50.0),
                    reset_at: None,
                })
                .collect(),
            metadata: serde_json::json!({"dataset_provenance": datasets}),
        };

        benchmark("core.windows_provenance.256", 2_000, || {
            snapshot.windows_provenance().len()
        });
    }

    fn snapshot(metadata: serde_json::Value, kind: UsageWindowKind) -> UsageSnapshot {
        UsageSnapshot {
            provider_id: ProviderId::new("opencode_go"),
            account_id: AccountId::new("account"),
            collected_at: Utc::now(),
            windows: vec![UsageWindow {
                window_id: "window".to_string(),
                label: "Window".to_string(),
                kind,
                used: None,
                limit: None,
                remaining: None,
                percent_used: Some(50.0),
                percent_remaining: Some(50.0),
                reset_at: None,
            }],
            metadata,
        }
    }

    #[test]
    fn synthetic_local_windows_are_explicitly_non_authoritative() {
        let snapshot = snapshot(
            serde_json::json!({"estimate": true, "web_authoritative": false}),
            UsageWindowKind::Weekly,
        );

        let provenance = snapshot.window_provenance(&snapshot.windows[0]);
        assert_eq!(provenance.source, UsageDataSource::SyntheticLocalEstimate);
        assert!(!provenance.authoritative);
        assert!(!snapshot.window_is_authoritative_quota(&snapshot.windows[0]));
    }

    #[test]
    fn credit_balances_are_not_quota_alert_inputs() {
        let snapshot = snapshot(serde_json::json!({}), UsageWindowKind::Credits);

        assert!(!snapshot.window_is_authoritative_quota(&snapshot.windows[0]));
    }

    #[test]
    fn indexed_provenance_preserves_first_dataset_match() {
        let mut datasets = (0..5)
            .map(|_| {
                serde_json::json!({
                    "authoritative": true,
                    "provenance": {
                        "source": "provider_reported",
                        "scope": "account_wide",
                        "quality": "authoritative",
                        "completeness": "complete",
                        "confidence": "high"
                    },
                    "window_ids": ["window"]
                })
            })
            .collect::<Vec<_>>();
        datasets[0]["authoritative"] = serde_json::json!(false);
        datasets[0]["provenance"]["source"] = serde_json::json!("local_logs");
        let snapshot = snapshot(
            serde_json::json!({"dataset_provenance": datasets}),
            UsageWindowKind::Weekly,
        );

        let provenance = snapshot.windows_provenance();

        assert_eq!(provenance[0].source, UsageDataSource::LocalLogs);
        assert!(!provenance[0].authoritative);
    }

    #[test]
    fn aggregate_preserves_unavailable_daily_cost() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();
        let provenance = DataProvenance {
            source: UsageDataSource::LocalLogs,
            scope: UsageDataScope::ThisDevice,
            quality: UsageDataQuality::Estimated,
            completeness: UsageDataCompleteness::Partial,
            confidence: UsageDataConfidence::Medium,
        };
        let dashboard = aggregate_usage_dashboard(vec![AccountUsageSummary {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("default"),
            activity: None,
            cost: Some(CostSummary {
                provenance,
                days: vec![DailyUsagePoint {
                    date,
                    tokens: 10,
                    cost_usd: None,
                    priced_tokens: 0,
                    unpriced_tokens: 10,
                }],
                today_cost_usd: 0.0,
                lookback_cost_usd: 0.0,
                pricing: PricingCoverage {
                    priced_tokens: 0,
                    unpriced_tokens: 10,
                    covered_percent: 0.0,
                    unpriced_models: vec!["unknown".to_string()],
                    catalog_version: None,
                    catalog_source: None,
                    catalog_effective_from: None,
                },
            }),
            reset_credits: None,
        }]);

        assert_eq!(dashboard.days[0].cost_usd, None);
    }
}
