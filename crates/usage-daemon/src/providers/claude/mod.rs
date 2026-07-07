use std::path::PathBuf;

use async_trait::async_trait;
use tokio::sync::Mutex;
use usage_core::ProviderId;

use crate::providers::{
    DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
    ProviderErrorKind, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
};

mod client;
mod cost;
mod credentials;
mod normalize;

use client::ClaudeApiClient;
use cost::{merge_local_cost_report, scan_claude_local_costs};
use credentials::{load_credentials, ClaudeCredentials};
use normalize::normalize_usage;

pub const PROVIDER_ID: &str = "claude";
const CLAUDE_CREDENTIALS_FILE: &str = ".claude/.credentials.json";
const CLAUDE_COLLECTION_MODE: &str = "oauth_usage_api";

pub struct ClaudeCollector {
    keychain_account: String,
    credentials_file_path: PathBuf,
    credentials_cache: Mutex<Option<ClaudeCredentials>>,
    api: ClaudeApiClient,
    capture_raw_payloads: bool,
}

impl ClaudeCollector {
    pub fn new(capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Claude data"))?;
        Ok(Self {
            keychain_account: std::env::var("USER").unwrap_or_else(|_| "default".to_string()),
            credentials_file_path: home.join(CLAUDE_CREDENTIALS_FILE),
            credentials_cache: Mutex::new(None),
            api: ClaudeApiClient::new(HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT)?,
            capture_raw_payloads,
        })
    }

    async fn load_credentials(&self) -> Result<ClaudeCredentials, ProviderError> {
        if let Some(credentials) = self.credentials_cache.lock().await.clone() {
            return Ok(credentials);
        }

        let credentials = load_credentials(
            self.keychain_account.clone(),
            self.credentials_file_path.clone(),
        )
        .await?;

        *self.credentials_cache.lock().await = Some(credentials.clone());
        Ok(credentials)
    }

    async fn refresh_credentials(
        &self,
        credentials: ClaudeCredentials,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let refreshed = self.api.refresh_credentials(credentials).await?;
        *self.credentials_cache.lock().await = Some(refreshed.clone());
        Ok(refreshed)
    }

    async fn load_with_auto_refresh(&self) -> Result<ClaudeCredentials, ProviderError> {
        let credentials = self.load_credentials().await?;
        if credentials.is_expired() {
            self.refresh_credentials(credentials).await
        } else {
            Ok(credentials)
        }
    }
}

#[async_trait]
impl ProviderCollector for ClaudeCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        let credentials = self.load_credentials().await?;
        Ok(vec![DiscoveredAccount {
            external_account_id: credentials.account_id(),
            display_name: Some(credentials.display_name()),
        }])
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let mut credentials = self.load_with_auto_refresh().await?;
        if credentials.account_id() != account.external_account_id {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Claude account changed since discovery",
            ));
        }

        let payload = match self.api.fetch_usage(&credentials).await {
            Err(err) if err.kind() == ProviderErrorKind::Unauthorized => {
                credentials = self.refresh_credentials(credentials).await?;
                self.api.fetch_usage(&credentials).await?
            }
            result => result?,
        };
        let mut usage = normalize_usage(&payload, &credentials)?;
        let mut warnings = Vec::new();
        match tokio::task::spawn_blocking(scan_claude_local_costs).await {
            Ok(Ok(report)) => merge_local_cost_report(&mut usage, report),
            Ok(Err(err)) => warnings.push(format!("Claude local cost scan failed: {err}")),
            Err(err) => warnings.push(format!("Claude local cost scan task failed: {err}")),
        }

        Ok(ProviderCollectionResult {
            usage,
            collection_mode: CLAUDE_COLLECTION_MODE.to_string(),
            raw_payload: self.capture_raw_payloads.then_some(payload),
            warnings,
        })
    }
}
