use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Account, AccountId, ProviderHealth, ProviderId, UsageSnapshot};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ApiRequest {
    GetUsage,
    Refresh { providers: Option<Vec<ProviderId>> },
    GetProviderHealth,
    GetAccounts,
    GetConfig,
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
    Error {
        error: ApiErrorResponse,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderRefreshResult {
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub status: String,
    pub collection_mode: Option<String>,
    pub collected_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfigResponse {
    pub poll_interval_seconds: u64,
    pub config_path: String,
    pub socket_path: String,
    pub db_path: String,
    pub enabled_providers: Vec<ProviderId>,
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
