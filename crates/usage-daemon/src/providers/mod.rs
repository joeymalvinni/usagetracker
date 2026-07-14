use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures_util::StreamExt;
use thiserror::Error;
use usage_core::{
    DataProvenance, ProviderFailureCode, ProviderId, UsageDataCompleteness, UsageDataConfidence,
    UsageDataQuality, UsageDataScope, UsageDataSource, UsageSnapshot, UsageWindow,
};

macro_rules! settings_accessors {
    (provider: $settings:ty) => {
        pub(crate) fn provider(
            config: &crate::config::ProviderConfig,
        ) -> anyhow::Result<$settings> {
            config.settings()
        }

        pub(crate) fn update_provider(
            config: &mut crate::config::ProviderConfig,
            mutation: impl FnOnce(&mut $settings),
        ) -> anyhow::Result<()> {
            let mut settings = provider(config)?;
            mutation(&mut settings);
            config.patch_settings(&settings)
        }
    };
    (profile: $settings:ty) => {
        pub(crate) fn profile(
            config: &crate::config::ProviderProfileConfig,
        ) -> anyhow::Result<$settings> {
            config.settings()
        }

        pub(crate) fn update_profile(
            config: &mut crate::config::ProviderProfileConfig,
            mutation: impl FnOnce(&mut $settings),
        ) -> anyhow::Result<()> {
            let mut settings = profile(config)?;
            mutation(&mut settings);
            config.patch_settings(&settings)
        }
    };
}

pub(crate) use settings_accessors;

pub(crate) mod browser_cookies;
pub mod claude;
pub mod codex;
pub mod grok;
pub(crate) mod launchers;
pub(crate) mod local_usage;
pub mod opencode;
pub(crate) mod paths;
pub(crate) mod profile_service;

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

#[derive(Debug)]
pub struct AccountDiscoveryFailure {
    pub profile_id: String,
    pub error: ProviderError,
}

#[derive(Debug)]
pub struct ProfileDiscovery {
    pub profile_id: String,
    pub result: Result<DiscoveredAccount, ProviderError>,
}

#[derive(Debug, Default)]
pub struct AccountDiscovery {
    pub profiles: Vec<ProfileDiscovery>,
}

impl From<Vec<DiscoveredAccount>> for AccountDiscovery {
    fn from(accounts: Vec<DiscoveredAccount>) -> Self {
        Self::from_parts(accounts, Vec::new())
    }
}

impl AccountDiscovery {
    pub fn from_parts(
        accounts: Vec<DiscoveredAccount>,
        failures: Vec<AccountDiscoveryFailure>,
    ) -> Self {
        let successes = accounts.into_iter().enumerate().map(|(index, account)| {
            let profile_id = account
                .profile_id
                .clone()
                .unwrap_or_else(|| default_profile_id(index));
            ProfileDiscovery {
                profile_id,
                result: Ok(account),
            }
        });
        let failures = failures.into_iter().map(|failure| ProfileDiscovery {
            profile_id: failure.profile_id,
            result: Err(failure.error),
        });
        Self {
            profiles: successes.chain(failures).collect(),
        }
    }

    pub fn into_parts(self) -> (Vec<DiscoveredAccount>, Vec<AccountDiscoveryFailure>) {
        let mut accounts = Vec::new();
        let mut failures = Vec::new();
        for profile in self.profiles {
            match profile.result {
                Ok(account) => accounts.push(account),
                Err(error) => failures.push(AccountDiscoveryFailure {
                    profile_id: profile.profile_id,
                    error,
                }),
            }
        }
        (accounts, failures)
    }
}

fn default_profile_id(index: usize) -> String {
    if index == 0 {
        "default".to_string()
    } else {
        format!("profile-{}", index + 1)
    }
}

impl FromIterator<DiscoveredAccount> for AccountDiscovery {
    fn from_iter<T: IntoIterator<Item = DiscoveredAccount>>(iter: T) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

#[derive(Clone, Debug)]
pub struct ProviderCollectionResult {
    pub usage: ProviderUsage,
    pub daily_usage: Vec<DailyUsageBucket>,
    pub collection_mode: String,
    pub account_email: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct UsageDataset {
    pub collection: ProviderCollectionResult,
    pub provenance: DataProvenance,
    pub authoritative: bool,
}

impl UsageDataset {
    pub fn authoritative(collection: ProviderCollectionResult) -> Self {
        Self {
            collection,
            provenance: DataProvenance {
                source: UsageDataSource::ProviderReported,
                scope: UsageDataScope::AccountWide,
                quality: UsageDataQuality::Authoritative,
                completeness: UsageDataCompleteness::Complete,
                confidence: UsageDataConfidence::High,
            },
            authoritative: true,
        }
    }

    pub fn supplemental(
        collection: ProviderCollectionResult,
        source: UsageDataSource,
        scope: UsageDataScope,
        quality: UsageDataQuality,
        completeness: UsageDataCompleteness,
    ) -> Self {
        Self {
            collection,
            provenance: DataProvenance {
                source,
                scope,
                quality,
                completeness,
                confidence: UsageDataConfidence::Medium,
            },
            authoritative: false,
        }
    }
}

#[derive(Debug)]
pub enum AuthoritativeOutcome {
    Collected(UsageDataset),
    #[allow(dead_code)] // Reserved for providers whose only meaningful data is supplemental.
    NotApplicable,
    Failed(ProviderError),
}

#[derive(Debug)]
pub struct CollectionOutcome {
    pub authoritative: AuthoritativeOutcome,
    pub supplemental: Vec<UsageDataset>,
}

impl CollectionOutcome {
    pub fn collected(collection: ProviderCollectionResult) -> Self {
        Self {
            authoritative: AuthoritativeOutcome::Collected(UsageDataset::authoritative(collection)),
            supplemental: Vec::new(),
        }
    }

    pub fn degraded(error: ProviderError, supplemental: Vec<UsageDataset>) -> Self {
        Self {
            authoritative: AuthoritativeOutcome::Failed(error),
            supplemental,
        }
    }
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

    /// Explicit configured profile identities. The coordinator uses this to
    /// enforce one discovery result per profile before touching storage.
    /// Profileless providers return an empty vector; requiring the method
    /// prevents a new multi-profile adapter from silently bypassing the
    /// discovery contract.
    fn configured_profile_ids(&self) -> Vec<String>;

    async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError>;

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<CollectionOutcome, ProviderError>;
}

pub type ProviderErrorKind = ProviderFailureCode;

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
