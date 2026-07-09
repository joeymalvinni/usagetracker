use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use usage_core::{ProviderId, UsageSnapshot, UsageWindow};

pub mod claude;
pub mod codex;
pub mod opencode;

pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct DiscoveredAccount {
    pub external_account_id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProviderCollectionResult {
    pub usage: ProviderUsage,
    pub collection_mode: String,
    pub account_display_name: Option<String>,
    pub raw_payload: Option<serde_json::Value>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProviderUsage {
    pub provider_id: ProviderId,
    pub collected_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    pub metadata: serde_json::Value,
}

impl ProviderUsage {
    pub fn into_snapshot(self, account_id: usage_core::AccountId) -> UsageSnapshot {
        UsageSnapshot {
            provider_id: self.provider_id,
            account_id,
            collected_at: self.collected_at,
            windows: self.windows,
            metadata: self.metadata,
        }
    }
}

#[async_trait]
pub trait ProviderCollector: Send + Sync {
    fn provider_id(&self) -> ProviderId;

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError>;

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderErrorKind {
    CredentialsMissing,
    CredentialsInvalid,
    Unauthorized,
    RateLimited,
    Network,
    Parse,
    ProviderUnavailable,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CredentialsMissing => "credentials_missing",
            Self::CredentialsInvalid => "credentials_invalid",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::Network => "network",
            Self::Parse => "parse",
            Self::ProviderUnavailable => "provider_unavailable",
        }
    }
}

#[derive(Debug, Error)]
#[error("{kind:?}: {message}")]
pub struct ProviderError {
    kind: ProviderErrorKind,
    message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    pub fn short_message(&self) -> &str {
        &self.message
    }
}
