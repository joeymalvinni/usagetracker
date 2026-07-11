use std::{collections::BTreeMap, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    Account, AccountId, ProviderHealth, ProviderId, RefreshJobId, UsageDashboardSummary,
    UsageForecast, UsageSnapshot, UsageWindowProvenance,
};

pub const API_VERSION: u16 = 2;
pub const MINIMUM_CLIENT_API_VERSION: u16 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestEnvelope {
    pub api_version: u16,
    #[serde(flatten)]
    pub request: ApiRequest,
}

impl RequestEnvelope {
    pub fn new(request: ApiRequest) -> Self {
        Self {
            api_version: API_VERSION,
            request,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResponseEnvelope {
    pub api_version: u16,
    #[serde(flatten)]
    pub response: ApiResponse,
}

impl ResponseEnvelope {
    pub fn new(response: ApiResponse) -> Self {
        Self {
            api_version: API_VERSION,
            response,
        }
    }

    pub fn error(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self::new(ApiResponse::error(code, message))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ApiRequest {
    GetServerInfo,
    GetUsage,
    Refresh {
        providers: Option<Vec<ProviderId>>,
    },
    GetRefreshJob {
        job_id: RefreshJobId,
    },
    GetProviderHealth,
    GetAccounts,
    GetConfig,
    GetPendingNotifications,
    AcknowledgeNotifications {
        ids: Vec<i64>,
    },
    UpdateConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        poll_interval_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        providers: Option<BTreeMap<String, ProviderToggle>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notifications: Option<NotificationConfig>,
    },
    AddProviderAccount {
        provider_id: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    UpdateAccount {
        account_id: AccountId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection_enabled: Option<bool>,
    },
    RemoveAccount {
        account_id: AccountId,
    },
    DeleteAccount {
        account_id: AccountId,
    },
    GetProviderSetup {
        provider_id: ProviderId,
    },
    UpdateProviderSetup {
        provider_id: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    RepairProvider {
        provider_id: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<AccountId>,
    },
    LaunchProviderAccount {
        account_id: AccountId,
    },
}

impl ApiRequest {
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::GetServerInfo => "get_server_info",
            Self::GetUsage => "get_usage",
            Self::Refresh { .. } => "refresh",
            Self::GetRefreshJob { .. } => "get_refresh_job",
            Self::GetProviderHealth => "get_provider_health",
            Self::GetAccounts => "get_accounts",
            Self::GetConfig => "get_config",
            Self::GetPendingNotifications => "get_pending_notifications",
            Self::AcknowledgeNotifications { .. } => "acknowledge_notifications",
            Self::UpdateConfig { .. } => "update_config",
            Self::AddProviderAccount { .. } => "add_provider_account",
            Self::UpdateAccount { .. } => "update_account",
            Self::RemoveAccount { .. } => "remove_account",
            Self::DeleteAccount { .. } => "delete_account",
            Self::GetProviderSetup { .. } => "get_provider_setup",
            Self::UpdateProviderSetup { .. } => "update_provider_setup",
            Self::RepairProvider { .. } => "repair_provider",
            Self::LaunchProviderAccount { .. } => "launch_provider_account",
        }
    }

    pub fn supports_method(method: &str) -> bool {
        matches!(
            method,
            "get_server_info"
                | "get_usage"
                | "refresh"
                | "get_refresh_job"
                | "get_provider_health"
                | "get_accounts"
                | "get_config"
                | "get_pending_notifications"
                | "acknowledge_notifications"
                | "update_config"
                | "add_provider_account"
                | "update_account"
                | "remove_account"
                | "delete_account"
                | "get_provider_setup"
                | "update_provider_setup"
                | "repair_provider"
                | "launch_provider_account"
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ProviderToggle {
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotificationConfig {
    #[serde(default = "default_notifications_enabled")]
    pub enabled: bool,
    #[serde(default = "default_notification_thresholds")]
    pub thresholds_percent_remaining: Vec<u8>,
    #[serde(default = "default_true")]
    pub reset_alerts: bool,
    #[serde(default)]
    pub predictive_alerts: bool,
    #[serde(default = "default_notification_cooldown_minutes")]
    pub cooldown_minutes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<NotificationQuietHours>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<NotificationRule>,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thresholds_percent_remaining: default_notification_thresholds(),
            reset_alerts: true,
            predictive_alerts: false,
            cooldown_minutes: default_notification_cooldown_minutes(),
            quiet_hours: None,
            rules: Vec::new(),
        }
    }
}

impl From<bool> for NotificationConfig {
    fn from(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }
}

impl NotificationConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_notification_thresholds(&self.thresholds_percent_remaining)?;
        if self.cooldown_minutes > 7 * 24 * 60 {
            return Err("notification cooldown cannot exceed seven days");
        }
        if let Some(hours) = &self.quiet_hours {
            hours.validate()?;
        }
        for rule in &self.rules {
            if rule.account_id.is_none() && rule.window_id.is_none() {
                return Err("notification rules must target an account or window");
            }
            if let Some(thresholds) = &rule.thresholds_percent_remaining {
                validate_notification_thresholds(thresholds)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotificationQuietHours {
    pub start_hour_local: u8,
    pub end_hour_local: u8,
}

impl NotificationQuietHours {
    fn validate(&self) -> Result<(), &'static str> {
        if self.start_hour_local > 23 || self.end_hour_local > 23 {
            return Err("quiet-hour values must be between 0 and 23");
        }
        if self.start_hour_local == self.end_hour_local {
            return Err("quiet hours cannot cover the entire day");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotificationRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<AccountId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds_percent_remaining: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_alerts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predictive_alerts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<DateTime<Utc>>,
}

fn validate_notification_thresholds(thresholds: &[u8]) -> Result<(), &'static str> {
    if thresholds.is_empty() || thresholds.len() > 7 {
        return Err("notifications require between one and seven thresholds");
    }
    if thresholds.iter().any(|threshold| *threshold > 100) {
        return Err("notification thresholds must be percentages from 0 through 100");
    }
    let mut sorted = thresholds.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != thresholds.len() {
        return Err("notification thresholds must be unique");
    }
    Ok(())
}

fn default_notifications_enabled() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_notification_thresholds() -> Vec<u8> {
    vec![50, 25, 10, 5, 0]
}

fn default_notification_cooldown_minutes() -> u32 {
    15
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResponse {
    ServerInfo {
        server: ServerInfo,
    },
    Usage {
        snapshots: Vec<UsageSnapshot>,
        dashboard: UsageDashboardSummary,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        forecasts: Vec<UsageForecast>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        window_provenance: Vec<UsageWindowProvenance>,
    },
    RefreshStarted {
        job: RefreshJob,
        coalesced: bool,
    },
    RefreshJob {
        job: RefreshJob,
    },
    ProviderHealth {
        health: Vec<ProviderHealth>,
    },
    Accounts {
        accounts: Vec<Account>,
    },
    Config {
        config: ConfigResponse,
    },
    PendingNotifications {
        notifications: Vec<PendingNotification>,
    },
    NotificationsAcknowledged {
        ids: Vec<i64>,
    },
    AddProviderAccount {
        account: AddProviderAccountResponse,
    },
    Account {
        account: Account,
    },
    AccountDeleted {
        account_id: AccountId,
    },
    ProviderSetup {
        setup: ProviderSetupResponse,
    },
    ProviderAction {
        action: ProviderActionResponse,
    },
    Error {
        error: ApiErrorResponse,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerInfo {
    pub api_version: u16,
    pub minimum_client_api_version: u16,
    pub capabilities: Vec<ApiCapability>,
    pub providers: Vec<ProviderDescriptor>,
}

impl ServerInfo {
    pub fn current() -> Self {
        Self {
            api_version: API_VERSION,
            minimum_client_api_version: MINIMUM_CLIENT_API_VERSION,
            capabilities: vec![
                ApiCapability::TypedErrors,
                ApiCapability::UsageProvenance,
                ApiCapability::RefreshJobs,
                ApiCapability::RefreshCoalescing,
                ApiCapability::DiagnosticMetadata,
            ],
            providers: vec![
                ProviderDescriptor::new("codex", "Codex", 60),
                ProviderDescriptor::new("claude", "Claude", 60),
                ProviderDescriptor::new("opencode_go", "OpenCode Go", 60),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiCapability {
    TypedErrors,
    UsageProvenance,
    RefreshJobs,
    RefreshCoalescing,
    DiagnosticMetadata,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: String,
    pub minimum_refresh_interval_seconds: u64,
}

impl ProviderDescriptor {
    fn new(id: &str, display_name: &str, minimum_refresh_interval_seconds: u64) -> Self {
        Self {
            id: ProviderId::new(id),
            display_name: display_name.to_string(),
            minimum_refresh_interval_seconds,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RefreshScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<ProviderId>>,
}

impl RefreshScope {
    pub fn all() -> Self {
        Self { providers: None }
    }

    pub fn providers(mut providers: Vec<ProviderId>) -> Self {
        providers.sort();
        providers.dedup();
        Self {
            providers: Some(providers),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTrigger {
    Manual,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl RefreshJobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RefreshJob {
    pub id: RefreshJobId,
    pub scope: RefreshScope,
    pub trigger: RefreshTrigger,
    pub status: RefreshJobStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_results: Vec<ProviderRefreshResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AddProviderAccountResponse {
    pub provider_id: ProviderId,
    pub profile_id: String,
    pub display_name: Option<String>,
    pub profile_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderSetupResponse {
    pub provider_id: ProviderId,
    pub profiles: Vec<ProviderProfileResponse>,
    pub selected_workspace_id: Option<String>,
    pub workspace_options: Vec<String>,
    pub discovery_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderProfileResponse {
    pub id: String,
    pub display_name: Option<String>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderActionResponse {
    pub provider_id: ProviderId,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderRefreshResult {
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub status: ProviderRefreshStatus,
    pub collection_mode: Option<String>,
    pub collected_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRefreshStatus {
    Ok,
    CredentialsMissing,
    CredentialsInvalid,
    Unauthorized,
    RateLimited,
    Network,
    Parse,
    ProviderUnavailable,
    StorageError,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfigResponse {
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub notifications: NotificationConfig,
    pub config_path: String,
    pub socket_path: String,
    pub db_path: String,
    pub enabled_providers: Vec<ProviderId>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderToggle>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingNotification {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidJson,
    InvalidRequest,
    RequestTooLarge,
    UnsupportedMethod,
    IncompatibleProtocol,
    InvalidArgument,
    UnknownProvider,
    UnknownAccount,
    UnknownRefreshJob,
    UnsupportedOperation,
    Conflict,
    StorageUnavailable,
    Internal,
}

impl ApiErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::InvalidRequest => "invalid_request",
            Self::RequestTooLarge => "request_too_large",
            Self::UnsupportedMethod => "unsupported_method",
            Self::IncompatibleProtocol => "incompatible_protocol",
            Self::InvalidArgument => "invalid_argument",
            Self::UnknownProvider => "unknown_provider",
            Self::UnknownAccount => "unknown_account",
            Self::UnknownRefreshJob => "unknown_refresh_job",
            Self::UnsupportedOperation => "unsupported_operation",
            Self::Conflict => "conflict",
            Self::StorageUnavailable => "storage_unavailable",
            Self::Internal => "internal",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(self, Self::StorageUnavailable | Self::Internal)
    }
}

impl fmt::Display for ApiErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiErrorResponse {
    pub code: ApiErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ApiResponse {
    pub fn error(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            error: ApiErrorResponse {
                code,
                message: message.into(),
                retryable: code.retryable(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_round_trips_with_flat_method() {
        let request: RequestEnvelope = serde_json::from_str(
            r#"{"api_version":2,"method":"launch_provider_account","account_id":"account-1"}"#,
        )
        .unwrap();

        assert_eq!(request.api_version, API_VERSION);
        match request.request {
            ApiRequest::LaunchProviderAccount { account_id } => {
                assert_eq!(account_id.as_str(), "account-1");
            }
            _ => panic!("unexpected request variant"),
        }
    }

    #[test]
    fn decodes_usage_response_without_optional_collections() {
        let response: ResponseEnvelope = serde_json::from_str(
            r#"{"api_version":2,"type":"usage","snapshots":[],"dashboard":{"accounts":[],"days":[],"pricing":{"priced_tokens":0,"unpriced_tokens":0,"covered_percent":100.0},"provenance":{"scopes":[],"qualities":[],"partial":false,"estimated":false,"mixed_scope":false,"explanation":"No usage data."}}}"#,
        )
        .unwrap();

        match response.response {
            ApiResponse::Usage {
                snapshots,
                forecasts,
                window_provenance,
                ..
            } => {
                assert!(snapshots.is_empty());
                assert!(forecasts.is_empty());
                assert!(window_provenance.is_empty());
            }
            _ => panic!("unexpected response variant"),
        }
    }

    #[test]
    fn fixture_server_info_decodes() {
        let response: ResponseEnvelope =
            serde_json::from_str(include_str!("../wire-fixtures/server_info_v2.json")).unwrap();
        let ApiResponse::ServerInfo { server } = response.response else {
            panic!("unexpected fixture response");
        };
        assert_eq!(server.api_version, API_VERSION);
        assert!(server.capabilities.contains(&ApiCapability::RefreshJobs));
    }

    #[test]
    fn fixture_refresh_job_decodes() {
        let response: ResponseEnvelope =
            serde_json::from_str(include_str!("../wire-fixtures/refresh_job_v2.json")).unwrap();
        let ApiResponse::RefreshJob { job } = response.response else {
            panic!("unexpected fixture response");
        };
        assert_eq!(job.status, RefreshJobStatus::Completed);
        assert_eq!(job.provider_results.len(), 1);
    }

    #[test]
    fn fixture_typed_error_decodes() {
        let response: ResponseEnvelope =
            serde_json::from_str(include_str!("../wire-fixtures/error_v2.json")).unwrap();
        let ApiResponse::Error { error } = response.response else {
            panic!("unexpected fixture response");
        };
        assert_eq!(error.code, ApiErrorCode::UnsupportedMethod);
        assert!(!error.retryable);
    }

    #[test]
    fn fixture_usage_provenance_decodes() {
        let response: ResponseEnvelope =
            serde_json::from_str(include_str!("../wire-fixtures/usage_v2.json")).unwrap();
        let ApiResponse::Usage {
            snapshots,
            window_provenance,
            ..
        } = response.response
        else {
            panic!("unexpected fixture response");
        };
        assert_eq!(snapshots.len(), 1);
        assert!(window_provenance[0].authoritative);
    }
}
