use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Account, AccountId, ProviderHealth, ProviderId, UsageSnapshot};

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
    UpdateConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        poll_interval_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        providers: Option<BTreeMap<String, ProviderToggle>>,
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
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ProviderToggle {
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResponse {
    Usage {
        snapshots: Vec<UsageSnapshot>,
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
    pub config_path: String,
    pub socket_path: String,
    pub db_path: String,
    pub enabled_providers: Vec<ProviderId>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderToggle>,
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
