use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use unicode_normalization::UnicodeNormalization;
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
use cost::{merge_local_cost_report, scan_claude_local_costs_cached, ClaudeCostCache};
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
    keychain_service: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
    config_dir: Option<PathBuf>,
    credentials_cache: Mutex<Option<ClaudeCredentials>>,
    display_name: Option<String>,
    cli_enabled: bool,
    project_roots: Vec<PathBuf>,
    cost_cache: Arc<StdMutex<Option<ClaudeCostCache>>>,
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
            profile.keychain_service.clone(),
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
        let refreshed = match self.api.refresh_credentials(credentials).await {
            Ok(refreshed) => refreshed,
            Err(err) => {
                *profile.credentials_cache.lock().await = None;
                return Err(err);
            }
        };
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
                match self.api.fetch_usage(&credentials).await {
                    Ok(payload) => payload,
                    Err(err) => {
                        if err.kind() == ProviderErrorKind::Unauthorized {
                            *profile.credentials_cache.lock().await = None;
                        }
                        return Err(err);
                    }
                }
            }
            result => result?,
        };
        let usage = normalize_usage(&payload, &credentials)?;
        Ok((usage, payload))
    }

    async fn collect_usage_with_cli(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<cli::ClaudeCliUsage, ProviderError> {
        let config_dir = profile.config_dir.clone();
        tokio::task::spawn_blocking(move || collect_usage_from_cli(config_dir.as_deref()))
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
            let config_dir = profile.claude_config_dir.map(expand_home_path);
            let mut project_roots = if profile.project_roots.is_empty() {
                config_dir
                    .as_ref()
                    .map(|root| vec![root.join("projects")])
                    .unwrap_or_default()
            } else {
                profile
                    .project_roots
                    .into_iter()
                    .map(expand_home_path)
                    .collect()
            };
            if profile.owns_default_claude_activity {
                project_roots.push(home.join(".config/claude/projects"));
                project_roots.push(home.join(".claude/projects"));
            }
            project_roots.sort();
            project_roots.dedup();
            let keychain_service = profile
                .keychain_service
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .or_else(|| config_dir.as_deref().map(keychain_service_for_config_dir))
                .unwrap_or_else(|| credentials::CLAUDE_KEYCHAIN_SERVICE.to_string());
            Arc::new(ClaudeProfile {
                id,
                keychain_service,
                keychain_account,
                credentials_file_path,
                config_dir,
                credentials_cache: Mutex::new(None),
                display_name: profile.display_name,
                cli_enabled: profile
                    .cli_enabled
                    .unwrap_or(!has_explicit_profiles || index == 0),
                project_roots,
                cost_cache: Arc::new(StdMutex::new(None)),
            })
        })
        .collect()
}

pub(crate) fn keychain_service_for_config_dir(config_dir: &Path) -> String {
    let normalized = config_dir.to_string_lossy().nfc().collect::<String>();
    let digest = Sha256::digest(normalized.as_bytes());
    let suffix = format!("{digest:x}");
    format!("{}-{}", credentials::CLAUDE_KEYCHAIN_SERVICE, &suffix[..8])
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

fn should_use_cli_fallback(cli_enabled: bool, api_error: &ProviderError) -> bool {
    cli_enabled && api_error.kind() != ProviderErrorKind::RateLimited
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
                    display_name: profile.display_name.clone(),
                    email: None,
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
                        email: None,
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
            accounts.retain(|account| {
                seen.insert((
                    account.profile_id.clone(),
                    account.external_account_id.clone(),
                ))
            });
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
        let (mut usage, collection_mode, raw_payload) =
            match self.collect_usage_with_api(&profile, account).await {
                Ok((usage, payload)) => (
                    usage,
                    CLAUDE_COLLECTION_MODE.to_string(),
                    self.capture_raw_payloads.then_some(payload),
                ),
                Err(api_err) if should_use_cli_fallback(profile.cli_enabled, &api_err) => {
                    match self.collect_usage_with_cli(&profile).await {
                        Ok(cli_usage) => {
                            warnings.push(format!(
                                "Claude OAuth usage API failed; used CLI fallback: {}",
                                api_err.short_message()
                            ));
                            (
                                cli_usage.usage,
                                CLAUDE_CLI_COLLECTION_MODE.to_string(),
                                self.capture_raw_payloads.then_some(cli_usage.raw_output),
                            )
                        }
                        Err(cli_err) => {
                            return Err(ProviderError::new(
                                api_err.kind(),
                                format!(
                                    "Claude OAuth usage API failed ({}); CLI fallback failed ({})",
                                    api_err.short_message(),
                                    cli_err.short_message()
                                ),
                            ));
                        }
                    }
                }
                Err(api_err) => return Err(api_err),
            };

        if profile.cli_enabled {
            let project_roots = profile.project_roots.clone();
            let cost_cache = profile.cost_cache.clone();
            match tokio::task::spawn_blocking(move || {
                scan_claude_local_costs_cached(cost_cache, project_roots)
            })
            .await
            {
                Ok(Ok(scan)) => {
                    let cache_status = scan.cache_status;
                    merge_local_cost_report(&mut usage, scan.report);
                    usage.metadata["claude_cost"]["scan_cache"] = json!(cache_status);
                }
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
            account_email: None,
            raw_payload,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::fs;

    #[test]
    fn matches_claude_codes_custom_config_keychain_service() {
        assert_eq!(
            keychain_service_for_config_dir(Path::new("/tmp/claude-profile")),
            "Claude Code-credentials-7182514b"
        );
    }

    #[test]
    fn rate_limits_do_not_launch_the_cli_fallback() {
        let rate_limited = ProviderError::new(
            ProviderErrorKind::RateLimited,
            "usage endpoint rate limited",
        );
        let unavailable = ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "endpoint unavailable",
        );

        assert!(!should_use_cli_fallback(true, &rate_limited));
        assert!(should_use_cli_fallback(true, &unavailable));
        assert!(!should_use_cli_fallback(false, &unavailable));
    }

    #[test]
    fn explicit_profiles_keep_independent_cli_config_directories() {
        let home = Path::new("/Users/test");
        let profiles = claude_profiles(
            ProviderConfig {
                enabled: true,
                profiles: vec![
                    ProviderProfileConfig {
                        id: Some("personal".to_string()),
                        claude_config_dir: Some(PathBuf::from("/profiles/personal")),
                        ..ProviderProfileConfig::default()
                    },
                    ProviderProfileConfig {
                        id: Some("work".to_string()),
                        claude_config_dir: Some(PathBuf::from("/profiles/work")),
                        cli_enabled: Some(true),
                        ..ProviderProfileConfig::default()
                    },
                ],
                ..ProviderConfig::default()
            },
            home,
        );

        assert_eq!(profiles.len(), 2);
        assert_eq!(
            profiles[0].config_dir.as_deref(),
            Some(Path::new("/profiles/personal"))
        );
        assert_eq!(
            profiles[1].config_dir.as_deref(),
            Some(Path::new("/profiles/work"))
        );
        assert_eq!(
            profiles[0].project_roots,
            vec![PathBuf::from("/profiles/personal/projects")]
        );
        assert_eq!(
            profiles[1].project_roots,
            vec![PathBuf::from("/profiles/work/projects")]
        );
        assert_ne!(profiles[0].keychain_service, profiles[1].keychain_service);
    }

    #[test]
    fn local_activity_scans_do_not_cross_profile_roots() {
        let base = std::env::temp_dir().join(format!("claude-activity-{}", uuid::Uuid::new_v4()));
        let personal = base.join("personal/projects/workspace");
        let work = base.join("work/projects/workspace");
        fs::create_dir_all(&personal).unwrap();
        fs::create_dir_all(&work).unwrap();
        write_usage_event(&personal.join("personal.jsonl"), "personal", 10, 1);
        write_usage_event(&work.join("work.jsonl"), "work", 20, 2);

        let personal_scan = scan_claude_local_costs_cached(
            Arc::new(StdMutex::new(None)),
            vec![base.join("personal/projects")],
        )
        .unwrap();
        let work_scan = scan_claude_local_costs_cached(
            Arc::new(StdMutex::new(None)),
            vec![base.join("work/projects")],
        )
        .unwrap();
        let mut personal_usage = empty_usage();
        let mut work_usage = empty_usage();
        merge_local_cost_report(&mut personal_usage, personal_scan.report);
        merge_local_cost_report(&mut work_usage, work_scan.report);

        assert_eq!(personal_usage.metadata["claude_cost"]["total_tokens"], 11);
        assert_eq!(work_usage.metadata["claude_cost"]["total_tokens"], 22);
        assert_eq!(personal_usage.metadata["claude_cost"]["files_scanned"], 1);
        assert_eq!(work_usage.metadata["claude_cost"]["files_scanned"], 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn default_activity_owner_scans_legacy_and_managed_roots() {
        let home = Path::new("/Users/test");
        let profiles = claude_profiles(
            ProviderConfig {
                enabled: true,
                profiles: vec![ProviderProfileConfig {
                    id: Some("personal".to_string()),
                    claude_config_dir: Some(PathBuf::from("/profiles/personal")),
                    owns_default_claude_activity: true,
                    ..ProviderProfileConfig::default()
                }],
                ..ProviderConfig::default()
            },
            home,
        );

        assert_eq!(
            profiles[0].project_roots,
            vec![
                PathBuf::from("/Users/test/.claude/projects"),
                PathBuf::from("/Users/test/.config/claude/projects"),
                PathBuf::from("/profiles/personal/projects"),
            ]
        );
    }

    fn empty_usage() -> ProviderUsage {
        ProviderUsage {
            provider_id: ProviderId::new(PROVIDER_ID),
            collected_at: Utc::now(),
            windows: Vec::new(),
            metadata: json!({}),
        }
    }

    fn write_usage_event(path: &Path, id: &str, input_tokens: u64, output_tokens: u64) {
        let event = json!({
            "type": "assistant",
            "timestamp": Utc::now().to_rfc3339(),
            "requestId": format!("req-{id}"),
            "message": {
                "id": format!("msg-{id}"),
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": input_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": output_tokens
                }
            }
        });
        fs::write(path, format!("{event}\n")).unwrap();
    }
}
