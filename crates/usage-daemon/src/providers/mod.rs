use std::{future::Future, pin::Pin};

use thiserror::Error;
use usage_core::{ProviderId, UsageSnapshot};

pub mod codex;

#[derive(Clone, Debug)]
pub struct DiscoveredAccount {
    pub external_account_id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProviderCollectionResult {
    pub snapshot: UsageSnapshot,
    pub collection_mode: String,
    pub raw_payload: Option<serde_json::Value>,
    pub warnings: Vec<String>,
}

pub trait ProviderCollector: Send + Sync {
    fn provider_id(&self) -> ProviderId;

    fn discover_accounts<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DiscoveredAccount>, ProviderError>> + Send + 'a>>;

    fn collect_usage<'a>(
        &'a self,
        account: &'a DiscoveredAccount,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCollectionResult, ProviderError>> + Send + 'a>>;
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
    Unsupported,
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
            Self::Unsupported => "unsupported",
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
