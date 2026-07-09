use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use usage_core::ProviderId;

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{
        DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
        ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    },
};

mod cli;
mod client;
mod cost;
mod credentials;
mod normalize;

use cli::collect_usage_from_cli;
use client::ClaudeApiClient;
use cost::{merge_local_cost_report, scan_claude_local_costs_from_roots};
use credentials::{load_credentials, ClaudeCredentials};
use normalize::normalize_usage;

pub const PROVIDER_ID: &str = "claude";
const CLAUDE_CREDENTIALS_FILE: &str = ".claude/.credentials.json";
const CLAUDE_COLLECTION_MODE: &str = "oauth_usage_api";
const CLAUDE_CLI_COLLECTION_MODE: &str = "claude_cli_usage";

pub struct ClaudeCollector {
    profiles: Vec<Arc<ClaudeProfile>>,
    api: ClaudeApiClient,
    capture_raw_payloads: bool,
}

struct ClaudeProfile {
    id: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
    credentials_cache: Mutex<Option<ClaudeCredentials>>,
    display_name: Option<String>,
    cli_enabled: bool,
    project_roots: Vec<PathBuf>,
}

impl ClaudeCollector {
    pub fn new(config: ProviderConfig, capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Claude data"))?;
        let profiles = claude_profiles(config, &home);
        Ok(Self {
            profiles,
            api: ClaudeApiClient::new(HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT)?,
            capture_raw_payloads,
        })
    }

    async fn load_credentials(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeCredentials, ProviderError> {
        if let Some(credentials) = profile.credentials_cache.lock().await.clone() {
            return Ok(credentials);
        }

        let credentials = load_credentials(
            profile.keychain_account.clone(),
            profile.credentials_file_path.clone(),
        )
        .await?;

        *profile.credentials_cache.lock().await = Some(credentials.clone());
        Ok(credentials)
    }

    async fn refresh_credentials(
        &self,
        profile: &ClaudeProfile,
        credentials: ClaudeCredentials,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let refreshed = self.api.refresh_credentials(credentials).await?;
        *profile.credentials_cache.lock().await = Some(refreshed.clone());
        Ok(refreshed)
    }

    async fn load_with_auto_refresh(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let credentials = self.load_credentials(profile).await?;
        if credentials.is_expired() {
            self.refresh_credentials(profile, credentials).await
        } else {
            Ok(credentials)
        }
    }

    async fn collect_usage_with_api(
        &self,
        profile: &ClaudeProfile,
        account: &DiscoveredAccount,
    ) -> Result<(ProviderUsage, serde_json::Value), ProviderError> {
        let mut credentials = self.load_with_auto_refresh(profile).await?;
        if credentials.account_id() != account.external_account_id {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Claude account changed since discovery",
            ));
        }

        let payload = match self.api.fetch_usage(&credentials).await {
            Err(err) if err.kind() == ProviderErrorKind::Unauthorized => {
                credentials = self.refresh_credentials(profile, credentials).await?;
                self.api.fetch_usage(&credentials).await?
            }
            result => result?,
        };
        let usage = normalize_usage(&payload, &credentials)?;
        Ok((usage, payload))
    }

    async fn collect_usage_with_cli(&self) -> Result<cli::ClaudeCliUsage, ProviderError> {
        tokio::task::spawn_blocking(collect_usage_from_cli)
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("Claude CLI usage task failed: {err}"),
                )
            })?
    }

    async fn profile_for_account(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<Arc<ClaudeProfile>, ProviderError> {
        if let Some(profile_id) = account.profile_id.as_deref() {
            return self
                .profiles
                .iter()
                .find(|profile| profile.id == profile_id)
                .cloned()
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        format!("Claude profile {profile_id} no longer exists"),
                    )
                });
        }
        self.profiles
            .iter()
            .find(|profile| profile.keychain_account == account.external_account_id)
            .cloned()
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "Claude account changed since discovery",
                )
            })
    }
}

fn claude_profiles(config: ProviderConfig, home: &Path) -> Vec<Arc<ClaudeProfile>> {
    let default_keychain_account = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    let has_explicit_profiles = !config.profiles.is_empty();
    let configured = if has_explicit_profiles {
        config.profiles
    } else {
        vec![ProviderProfileConfig {
            id: Some("default".to_string()),
            keychain_account: Some(default_keychain_account.clone()),
            credentials_file: Some(home.join(CLAUDE_CREDENTIALS_FILE)),
            cli_enabled: Some(true),
            ..ProviderProfileConfig::default()
        }]
    };

    configured
        .into_iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, profile)| {
            let id = profile_id(profile.id.as_deref(), index);
            let keychain_account = profile
                .keychain_account
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    if has_explicit_profiles {
                        id.clone()
                    } else {
                        default_keychain_account.clone()
                    }
                });
            let credentials_file_path = profile
                .credentials_file
                .map(expand_home_path)
                .unwrap_or_else(|| home.join(CLAUDE_CREDENTIALS_FILE));
            Arc::new(ClaudeProfile {
                id,
                keychain_account,
                credentials_file_path,
                credentials_cache: Mutex::new(None),
                display_name: profile.display_name,
                cli_enabled: profile
                    .cli_enabled
                    .unwrap_or(!has_explicit_profiles || index == 0),
                project_roots: profile
                    .project_roots
                    .into_iter()
                    .map(expand_home_path)
                    .collect(),
            })
        })
        .collect()
}

fn profile_id(configured: Option<&str>, index: usize) -> String {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if index == 0 {
                "default".to_string()
            } else {
                format!("profile-{}", index + 1)
            }
        })
}

fn expand_home_path(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if value == "~" {
        return dirs::home_dir().unwrap_or(path);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path
}

#[async_trait]
impl ProviderCollector for ClaudeCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        if self.profiles.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "no enabled Claude profiles are configured",
            ));
        }

        let mut accounts = Vec::new();
        let mut failures = Vec::new();
        for profile in &self.profiles {
            match self.load_credentials(profile).await {
                Ok(credentials) => accounts.push(DiscoveredAccount {
                    external_account_id: credentials.account_id(),
                    display_name: profile
                        .display_name
                        .clone()
                        .or_else(|| Some(credentials.display_name())),
                    profile_id: Some(profile.id.clone()),
                }),
                Err(err)
                    if matches!(
                        err.kind(),
                        ProviderErrorKind::CredentialsMissing
                            | ProviderErrorKind::CredentialsInvalid
                    ) =>
                {
                    accounts.push(DiscoveredAccount {
                        external_account_id: profile.keychain_account.clone(),
                        display_name: profile.display_name.clone(),
                        profile_id: Some(profile.id.clone()),
                    });
                }
                Err(err) => {
                    failures.push(err);
                }
            }
        }

        if !accounts.is_empty() {
            let mut seen = BTreeSet::new();
            accounts.retain(|account| seen.insert(account.external_account_id.clone()));
            return Ok(accounts);
        }
        Err(failures.into_iter().next().unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "no Claude accounts were discovered",
            )
        }))
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let profile = self.profile_for_account(account).await?;

        let mut warnings = Vec::new();
        let (mut usage, collection_mode, raw_payload) = if profile.cli_enabled {
            match self.collect_usage_with_cli().await {
                Ok(cli_usage) => (
                    cli_usage.usage,
                    CLAUDE_CLI_COLLECTION_MODE.to_string(),
                    self.capture_raw_payloads.then_some(cli_usage.raw_output),
                ),
                Err(cli_err) => match self.collect_usage_with_api(&profile, account).await {
                    Ok((usage, payload)) => {
                        warnings.push(format!(
                            "Claude CLI usage failed; used OAuth usage API fallback: {}",
                            cli_err.short_message()
                        ));
                        (
                            usage,
                            CLAUDE_COLLECTION_MODE.to_string(),
                            self.capture_raw_payloads.then_some(payload),
                        )
                    }
                    Err(api_err) => {
                        return Err(ProviderError::new(
                            cli_err.kind(),
                            format!(
                                "Claude CLI usage failed ({}); OAuth usage API fallback failed ({})",
                                cli_err.short_message(),
                                api_err.short_message()
                            ),
                        ));
                    }
                },
            }
        } else {
            match self.collect_usage_with_api(&profile, account).await {
                Ok((usage, payload)) => (
                    usage,
                    CLAUDE_COLLECTION_MODE.to_string(),
                    self.capture_raw_payloads.then_some(payload),
                ),
                Err(api_err) => return Err(api_err),
            }
        };

        if profile.cli_enabled {
            let project_roots = profile.project_roots.clone();
            match tokio::task::spawn_blocking(move || {
                scan_claude_local_costs_from_roots(project_roots)
            })
            .await
            {
                Ok(Ok(report)) => merge_local_cost_report(&mut usage, report),
                Ok(Err(err)) => warnings.push(format!("Claude local cost scan failed: {err}")),
                Err(err) => warnings.push(format!("Claude local cost scan task failed: {err}")),
            }
        }

        usage.metadata["credential_profile"] = json!(account.external_account_id);
        usage.metadata["profile_id"] = json!(profile.id.as_str());
        if let Some(display_name) = profile.display_name.as_deref() {
            usage.metadata["profile_display_name"] = json!(display_name);
        }
        if usage
            .metadata
            .get("subscription_type")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            if let Ok(credentials) = self.load_credentials(&profile).await {
                usage.metadata["subscription_type"] = json!(credentials.subscription_type);
                usage.metadata["rate_limit_tier"] = json!(credentials.rate_limit_tier);
            }
        }

        Ok(ProviderCollectionResult {
            usage,
            daily_usage: Vec::new(),
            collection_mode,
            account_display_name: None,
            raw_payload,
            warnings,
        })
    }
}
