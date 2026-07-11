use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures_util::StreamExt;
use thiserror::Error;
use usage_core::{ProviderId, UsageSnapshot, UsageWindow};

pub mod claude;
pub mod codex;
pub(crate) mod local_usage;
pub mod opencode;
pub(crate) mod paths;

pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
pub const MAX_PROVIDER_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub async fn read_response_body(
    response: reqwest::Response,
    label: &str,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            format!("{label} exceeded the {MAX_PROVIDER_RESPONSE_BYTES}-byte response limit"),
        ));
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_PROVIDER_RESPONSE_BYTES as u64) as usize,
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Network,
                format!("failed to read {label}: {err}"),
            )
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                format!("{label} exceeded the {MAX_PROVIDER_RESPONSE_BYTES}-byte response limit"),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Parses the standard `Retry-After` response header as either delta seconds or
/// an HTTP date. Invalid or past values are ignored instead of extending a
/// provider outage indefinitely.
pub fn retry_after_deadline(headers: &reqwest::header::HeaderMap) -> Option<DateTime<Utc>> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        let seconds = i64::try_from(seconds).ok()?;
        return Utc::now().checked_add_signed(chrono::TimeDelta::seconds(seconds));
    }

    let deadline = DateTime::parse_from_rfc2822(value)
        .or_else(|_| DateTime::parse_from_rfc3339(value))
        .ok()?
        .with_timezone(&Utc);
    (deadline > Utc::now()).then_some(deadline)
}

#[derive(Clone, Debug)]
pub struct DiscoveredAccount {
    pub external_account_id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProviderCollectionResult {
    pub usage: ProviderUsage,
    pub daily_usage: Vec<DailyUsageBucket>,
    pub collection_mode: String,
    pub account_email: Option<String>,
    pub raw_payload: Option<serde_json::Value>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DailyUsageBucket {
    pub date: NaiveDate,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    pub source: String,
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
    retry_at: Option<DateTime<Utc>>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_at: None,
        }
    }

    pub fn with_retry_at(mut self, retry_at: Option<DateTime<Utc>>) -> Self {
        self.retry_at = retry_at;
        self
    }

    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    pub fn short_message(&self) -> &str {
        &self.message
    }

    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        self.retry_at
    }
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;

    #[test]
    fn parses_retry_after_delta_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        let before = Utc::now() + chrono::TimeDelta::seconds(119);
        let after = Utc::now() + chrono::TimeDelta::seconds(121);
        let deadline = retry_after_deadline(&headers).unwrap();
        assert!(deadline >= before && deadline <= after);
    }
}
