use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{AccountId, ProviderId};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountDisplayNameSource {
    Provider,
    #[default]
    Generated,
    User,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageSnapshot {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub collected_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastStatus {
    InsufficientData,
    Safe,
    OnPace,
    AtRisk,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastConfidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageAmount {
    pub value: f64,
    pub unit: UsageUnit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageUnit {
    Tokens,
    Requests,
    Credits,
    Usd,
    Percent,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthStatus {
    Ok,
    CredentialsMissing,
    AuthFailed,
    RateLimited,
    ProviderError,
    ParseError,
    BackingOff,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
