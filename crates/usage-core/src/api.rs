use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Account, AccountId, ProviderHealth, ProviderId, UsageForecast, UsageSnapshot};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ApiRequest {
    GetUsage,
    Refresh {
        providers: Option<Vec<ProviderId>>,
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ProviderToggle {
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotificationConfig {
    #[serde(default = "default_notifications_enabled")]
    pub enabled: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_notifications_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResponse {
    Usage {
        snapshots: Vec<UsageSnapshot>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        forecasts: Vec<UsageForecast>,
    },
    Refresh {
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        provider_results: Vec<ProviderRefreshResult>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiErrorResponse {
    pub code: String,
    pub message: String,
}

impl ApiResponse {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            error: ApiErrorResponse {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_profile_session_launch_request() {
        let request: ApiRequest = serde_json::from_str(
            r#"{"method":"launch_provider_account","account_id":"account-1"}"#,
        )
        .unwrap();

        match request {
            ApiRequest::LaunchProviderAccount { account_id } => {
                assert_eq!(account_id.as_str(), "account-1");
            }
            _ => panic!("unexpected request variant"),
        }
    }

    #[test]
    fn decodes_usage_response_without_forecasts() {
        let response: ApiResponse =
            serde_json::from_str(r#"{"type":"usage","snapshots":[]}"#).unwrap();

        match response {
            ApiResponse::Usage {
                snapshots,
                forecasts,
            } => {
                assert!(snapshots.is_empty());
                assert!(forecasts.is_empty());
            }
            _ => panic!("unexpected response variant"),
        }
    }
}
