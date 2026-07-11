use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use unicode_normalization::UnicodeNormalization;
use usage_core::ProviderId;

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{
        paths::expand_home_path, DiscoveredAccount, ProviderCollectionResult, ProviderCollector,
        ProviderError, ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT,
        HTTP_REQUEST_TIMEOUT,
    },
};

mod cli;
mod client;
mod cost;
mod credentials;
mod normalize;
mod pricing;

use cli::collect_usage_from_cli;
use client::{parse_cached_profile_identity, ClaudeAccountIdentity, ClaudeApiClient};
use cost::{merge_local_cost_report, scan_claude_local_costs_cached, ClaudeCostCache};
use credentials::{load_credentials, ClaudeCredentials};
use normalize::normalize_usage;

pub const PROVIDER_ID: &str = "claude";
const CLAUDE_CREDENTIALS_FILE: &str = ".claude/.credentials.json";
const CLAUDE_COLLECTION_MODE: &str = "oauth_usage_api";
const CLAUDE_CLI_COLLECTION_MODE: &str = "claude_cli_usage";
const CLAUDE_CLI_PARSE_RETRY_DELAY: Duration = Duration::from_millis(500);

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
    identity_file_path: PathBuf,
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

    async fn reload_credentials(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeCredentials, ProviderError> {
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
        let access_token_expired = credentials.is_expired();
        debug!(
            provider_id = PROVIDER_ID,
            profile_id = profile.id,
            credential_source = credentials.source_label(),
            access_token_expired,
            token_expires_at_ms = credentials.expires_at_ms,
            "Claude OAuth credentials loaded"
        );
        if access_token_expired {
            info!(
                provider_id = PROVIDER_ID,
                profile_id = profile.id,
                recovery_stage = "oauth_expired_token_refresh",
                credential_source = credentials.source_label(),
                token_expires_at_ms = credentials.expires_at_ms,
                "Claude OAuth access token is expired; refreshing credentials"
            );
            self.refresh_credentials(profile, credentials).await
        } else {
            Ok(credentials)
        }
    }

    async fn reload_with_auto_refresh(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let credentials = self.reload_credentials(profile).await?;
        if credentials.is_expired() {
            self.refresh_credentials(profile, credentials).await
        } else {
            Ok(credentials)
        }
    }

    async fn fetch_profile_identity(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeAccountIdentity, ProviderError> {
        let mut credentials = self.reload_with_auto_refresh(profile).await?;
        let fetched = match self.api.fetch_profile(&credentials).await {
            Err(err) if err.kind() == ProviderErrorKind::Unauthorized => {
                credentials = self.refresh_credentials(profile, credentials).await?;
                self.api.fetch_profile(&credentials).await
            }
            result => result,
        };

        match fetched {
            Ok(identity) => Ok(identity),
            Err(primary) if !should_use_cached_identity(&credentials.scopes) => Err(primary),
            Err(primary) => match tokio::fs::read(&profile.identity_file_path).await {
                Ok(body) => parse_cached_profile_identity(&body).map_err(|cached| {
                    ProviderError::new(
                        primary.kind(),
                        format!(
                            "{}; cached Claude account identity was invalid ({})",
                            primary.short_message(),
                            cached.short_message()
                        ),
                    )
                }),
                Err(err) => Err(ProviderError::new(
                    primary.kind(),
                    format!(
                        "{}; cached Claude account identity could not be read from {} ({err})",
                        primary.short_message(),
                        profile.identity_file_path.display()
                    ),
                )),
            },
        }
    }

    async fn collect_usage_with_api(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<(ProviderUsage, serde_json::Value), ProviderError> {
        let mut credentials = self.load_with_auto_refresh(profile).await?;
        let payload = match self.api.fetch_usage(&credentials).await {
            Err(err) if err.kind() == ProviderErrorKind::Unauthorized => {
                warn!(
                    provider_id = PROVIDER_ID,
                    profile_id = profile.id,
                    recovery_stage = "oauth_usage_unauthorized_refresh",
                    error_code = err.kind().as_str(),
                    error = %err,
                    "Claude OAuth usage rejected the access token; refreshing credentials and retrying"
                );
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
        let started = Instant::now();
        let first = self.collect_usage_with_cli_once(profile).await;
        let Err(err) = &first else {
            return first;
        };
        if err.kind() != ProviderErrorKind::Parse {
            return first;
        }

        warn!(
            provider_id = PROVIDER_ID,
            profile_id = profile.id,
            attempt = 1,
            max_attempts = 2,
            recovery_stage = "cli_retry_scheduled",
            retry_delay_ms = CLAUDE_CLI_PARSE_RETRY_DELAY.as_millis(),
            elapsed_ms = started.elapsed().as_millis(),
            error_code = err.kind().as_str(),
            error = %err,
            "Claude CLI usage parse failed; retrying before OAuth fallback"
        );
        tokio::time::sleep(CLAUDE_CLI_PARSE_RETRY_DELAY).await;
        let retry_started = Instant::now();
        let retry = self.collect_usage_with_cli_once(profile).await;
        match &retry {
            Ok(usage) => info!(
                provider_id = PROVIDER_ID,
                profile_id = profile.id,
                recovery_stage = "cli_retry_recovered",
                recovered_attempt = 2,
                windows = usage.usage.windows.len(),
                retry_elapsed_ms = retry_started.elapsed().as_millis(),
                total_elapsed_ms = started.elapsed().as_millis(),
                "Claude CLI usage recovered after retry"
            ),
            Err(retry_err) => warn!(
                provider_id = PROVIDER_ID,
                profile_id = profile.id,
                recovery_stage = "cli_retry_exhausted",
                attempts = 2,
                retry_elapsed_ms = retry_started.elapsed().as_millis(),
                total_elapsed_ms = started.elapsed().as_millis(),
                initial_error_code = err.kind().as_str(),
                initial_error = %err,
                retry_error_code = retry_err.kind().as_str(),
                retry_error = %retry_err,
                "Claude CLI usage retry exhausted"
            ),
        }
        retry
    }

    async fn collect_usage_with_cli_once(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<cli::ClaudeCliUsage, ProviderError> {
        let config_dir = profile.config_dir.clone();
        let profile_id = profile.id.clone();
        tokio::task::spawn_blocking(move || {
            collect_usage_from_cli(config_dir.as_deref(), &profile_id)
        })
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
        Err(ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude account is missing its profile identity",
        ))
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
            let identity_file_path = config_dir
                .as_ref()
                .map(|root| root.join(".claude.json"))
                .unwrap_or_else(|| home.join(".claude.json"));
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
                identity_file_path,
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

fn should_use_cli_fallback(cli_enabled: bool, api_error: &ProviderError) -> bool {
    cli_enabled && api_error.kind() != ProviderErrorKind::RateLimited
}

fn deduplicate_accounts(accounts: &mut Vec<DiscoveredAccount>) {
    let mut canonical_profiles: BTreeMap<String, String> = BTreeMap::new();
    accounts.retain(|account| {
        let profile_id = account.profile_id.as_deref().unwrap_or("unknown");
        if let Some(canonical_profile_id) = canonical_profiles.get(&account.external_account_id) {
            warn!(
                external_account_id = account.external_account_id.as_str(),
                canonical_profile_id = canonical_profile_id.as_str(),
                duplicate_profile_id = profile_id,
                "duplicate Claude account ignored; each account can only be connected once"
            );
            false
        } else {
            canonical_profiles.insert(account.external_account_id.clone(), profile_id.to_string());
            true
        }
    });
}

fn should_use_cached_identity(scopes: &[String]) -> bool {
    !scopes
        .iter()
        .flat_map(|scope| scope.split_whitespace())
        .any(|scope| scope == "user:profile")
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
            match self.fetch_profile_identity(profile).await {
                Ok(identity) => accounts.push(DiscoveredAccount {
                    external_account_id: identity.account_id,
                    display_name: profile.display_name.clone(),
                    email: identity.email,
                    profile_id: Some(profile.id.clone()),
                }),
                Err(err) => {
                    failures.push(err);
                }
            }
        }

        if !accounts.is_empty() {
            deduplicate_accounts(&mut accounts);
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
            match self.collect_usage_with_api(&profile).await {
                Ok((usage, payload)) => (
                    usage,
                    CLAUDE_COLLECTION_MODE.to_string(),
                    self.capture_raw_payloads.then_some(payload),
                ),
                Err(api_err) if should_use_cli_fallback(profile.cli_enabled, &api_err) => {
                    warn!(
                        provider_id = PROVIDER_ID,
                        profile_id = profile.id,
                        credential_account = account.external_account_id,
                        recovery_stage = "cli_fallback_started",
                        oauth_error_code = api_err.kind().as_str(),
                        oauth_error = %api_err,
                        "Claude OAuth usage unavailable; starting CLI fallback"
                    );
                    let fallback_started = Instant::now();
                    match self.collect_usage_with_cli(&profile).await {
                        Ok(cli_usage) => {
                            info!(
                                provider_id = PROVIDER_ID,
                                profile_id = profile.id,
                                credential_account = account.external_account_id,
                                recovery_stage = "cli_fallback_succeeded",
                                windows = cli_usage.usage.windows.len(),
                                elapsed_ms = fallback_started.elapsed().as_millis(),
                                collection_mode = CLAUDE_CLI_COLLECTION_MODE,
                                "Claude CLI usage fallback succeeded"
                            );
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
                            warn!(
                                provider_id = PROVIDER_ID,
                                profile_id = profile.id,
                                credential_account = account.external_account_id,
                                recovery_stage = "cli_fallback_failed",
                                elapsed_ms = fallback_started.elapsed().as_millis(),
                                oauth_error_code = api_err.kind().as_str(),
                                oauth_error = %api_err,
                                cli_error_code = cli_err.kind().as_str(),
                                cli_error = %cli_err,
                                "Claude OAuth usage and CLI fallback both failed"
                            );
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
                    let cache_status = scan.cache_status.as_str();
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
            account_email: account.email.clone(),
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
        assert_eq!(
            profiles[0].identity_file_path,
            PathBuf::from("/profiles/personal/.claude.json")
        );
        assert_eq!(
            profiles[1].identity_file_path,
            PathBuf::from("/profiles/work/.claude.json")
        );
    }

    #[test]
    fn duplicate_account_uuid_keeps_first_configured_profile() {
        let account_uuid = "986efbc1-2be6-407a-9bcc-2e429b8e358d";
        let mut accounts = vec![
            DiscoveredAccount {
                external_account_id: account_uuid.to_string(),
                display_name: Some("First nickname".to_string()),
                email: Some("person@example.com".to_string()),
                profile_id: Some("first".to_string()),
            },
            DiscoveredAccount {
                external_account_id: account_uuid.to_string(),
                display_name: Some("Different nickname".to_string()),
                email: Some("person@example.com".to_string()),
                profile_id: Some("second".to_string()),
            },
            DiscoveredAccount {
                external_account_id: "23a6eae5-64a5-4424-bcf1-6e6527f8859d".to_string(),
                display_name: Some("Actually distinct".to_string()),
                email: Some("other@example.com".to_string()),
                profile_id: Some("distinct".to_string()),
            },
        ];

        deduplicate_accounts(&mut accounts);

        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].profile_id.as_deref(), Some("first"));
        assert_eq!(accounts[0].display_name.as_deref(), Some("First nickname"));
        assert_eq!(accounts[1].profile_id.as_deref(), Some("distinct"));
    }

    #[test]
    fn cached_identity_is_only_used_for_legacy_tokens_without_profile_scope() {
        assert!(should_use_cached_identity(&[]));
        assert!(should_use_cached_identity(&["user:inference".to_string()]));
        assert!(!should_use_cached_identity(&[
            "user:inference".to_string(),
            " user:profile ".to_string(),
        ]));
        assert!(!should_use_cached_identity(&[
            "user:inference user:profile".to_string()
        ]));
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
