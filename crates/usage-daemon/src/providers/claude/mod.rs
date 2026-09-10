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
use usage_core::{
    Account, ProviderId, SnapshotDetail, UsageDataCompleteness, UsageDataQuality, UsageDataScope,
    UsageDataSource, UsageSnapshot,
};

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    keychain,
    providers::{
        paths::expand_home_path, AccountDiscovery, AccountDiscoveryFailure, CollectionOutcome,
        DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
        ProviderErrorKind, ProviderUsage, UsageDataset, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    },
};

pub(crate) mod adapter;
mod cli;
mod client;
mod cost;
mod credentials;
pub(crate) mod local_import;
mod normalize;
mod pricing;
pub(crate) mod settings;

use cli::collect_usage_from_cli;
use client::{parse_cached_profile_identity, ClaudeAccountIdentity, ClaudeApiClient};
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
}

struct ClaudeProfile {
    id: String,
    keychain_service: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
    config_dir: Option<PathBuf>,
    identity_file_path: PathBuf,
    credentials_cache: Mutex<Option<(ClaudeCredentials, Instant)>>,
    display_name: Option<String>,
    cli_enabled: bool,
    project_roots: Vec<PathBuf>,
    cost_cache: Arc<StdMutex<Option<ClaudeCostCache>>>,
}

impl ClaudeCollector {
    pub fn new(config: ProviderConfig) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Claude data"))?;
        let profiles = claude_profiles(config, &home)?;
        Ok(Self {
            profiles,
            api: ClaudeApiClient::new(HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT)?,
        })
    }

    async fn load_credentials(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let mut cache = profile.credentials_cache.lock().await;
        if let Some((credentials, loaded_at)) = cache.as_ref() {
            if loaded_at.elapsed() < Duration::from_secs(60) {
                return Ok(credentials.clone());
            }
        }

        let credentials = load_credentials(
            profile.keychain_service.clone(),
            profile.keychain_account.clone(),
            profile.credentials_file_path.clone(),
        )
        .await?;

        *cache = Some((credentials.clone(), Instant::now()));
        Ok(credentials)
    }

    async fn reload_credentials(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let mut cache = profile.credentials_cache.lock().await;
        let credentials = load_credentials(
            profile.keychain_service.clone(),
            profile.keychain_account.clone(),
            profile.credentials_file_path.clone(),
        )
        .await?;
        *cache = Some((credentials.clone(), Instant::now()));
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
                return Err(self.recover_rejected_credentials(profile, err).await);
            }
        };
        *profile.credentials_cache.lock().await = Some((refreshed.clone(), Instant::now()));
        Ok(refreshed)
    }

    // Only definitive rejection discards an Allow-once value. Re-read after
    // eviction so a protected replacement is presented as permission recovery.
    async fn recover_rejected_credentials(
        &self,
        profile: &ClaudeProfile,
        error: ProviderError,
    ) -> ProviderError {
        if error.kind() != ProviderErrorKind::Unauthorized {
            return error;
        }
        if let Err(invalidation) = self.invalidate_cached_credentials(Some(&profile.id)).await {
            return invalidation;
        }
        match self.load_credentials(profile).await {
            Err(access) if access.kind() == ProviderErrorKind::KeychainAccessFailed => access,
            _ => error,
        }
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

    async fn collect_local_usage_dataset(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<Option<UsageDataset>, ProviderError> {
        if !profile.cli_enabled {
            return Ok(None);
        }
        let project_roots = profile.project_roots.clone();
        let cost_cache = profile.cost_cache.clone();
        let scan = tokio::task::spawn_blocking(move || {
            scan_claude_local_costs_cached(cost_cache, project_roots)
        })
        .await
        .map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("Claude local cost scan task failed: {err}"),
            )
        })?
        .map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Claude local cost scan failed: {err}"),
            )
        })?;

        let cache_status = scan.cache_status.as_str();
        let mut usage = ProviderUsage {
            provider_id: ProviderId::new(PROVIDER_ID),
            collected_at: chrono::Utc::now(),
            windows: Vec::new(),
            detail: SnapshotDetail::default(),
        };
        merge_local_cost_report(&mut usage, scan.report);
        if let Some(cost) = usage.detail.cost.as_mut() {
            cost.extra
                .insert("scan_cache".to_string(), json!(cache_status));
        }
        Ok(Some(UsageDataset::supplemental_named(
            "claude_local_logs",
            ProviderCollectionResult {
                usage,
                daily_usage: Vec::new(),
                usage_events: None,
                collection_mode: "claude_local_logs".to_string(),
                account_email: None,
                warnings: Vec::new(),
            },
            UsageDataSource::LocalLogs,
            UsageDataScope::SelectedLocalRoots,
            UsageDataQuality::Estimated,
            UsageDataCompleteness::Partial,
        )))
    }

    async fn fetch_profile_identity(
        &self,
        profile: &ClaudeProfile,
    ) -> Result<ClaudeAccountIdentity, ProviderError> {
        let mut credentials = match self.reload_with_auto_refresh(profile).await {
            Ok(credentials) => credentials,
            Err(error)
                if error.kind() == ProviderErrorKind::KeychainAccessFailed
                    && profile.cli_enabled
                    && supports_native_cli_auth(profile) =>
            {
                // Claude Code may already have access even when our helper does not.
                // Use only this CLI home's UUID; storage rejects identity changes.
                return tokio::fs::read(&profile.identity_file_path)
                    .await
                    .ok()
                    .and_then(|body| parse_cached_profile_identity(&body).ok())
                    .ok_or(error);
            }
            Err(error) => return Err(error),
        };
        let fetched = match self.api.fetch_profile(&credentials).await {
            Err(err) if err.kind() == ProviderErrorKind::Unauthorized => {
                credentials = self.refresh_credentials(profile, credentials).await?;
                self.api.fetch_profile(&credentials).await
            }
            result => result,
        };

        match fetched {
            Ok(identity) => Ok(identity),
            // A legacy token without user:profile may legitimately be denied
            // by this endpoint while its usage token remains valid.
            Err(primary)
                if primary.kind() == ProviderErrorKind::Unauthorized
                    && !should_use_cached_identity(&credentials.scopes) =>
            {
                Err(self.recover_rejected_credentials(profile, primary).await)
            }
            Err(primary) if primary.kind() == ProviderErrorKind::RateLimited => Err(primary),
            Err(primary) if !can_use_cached_identity(profile, &credentials) => Err(primary),
            Err(primary)
                if !should_use_cached_identity(&credentials.scopes)
                    && !should_use_cli_fallback(profile.cli_enabled, &primary) =>
            {
                Err(primary)
            }
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
                        return Err(self.recover_rejected_credentials(profile, err).await);
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
        expected_account_id: &str,
    ) -> Result<cli::ClaudeCliUsage, ProviderError> {
        let config_dir = profile.config_dir.clone();
        let profile_id = profile.id.clone();
        // Prefer the selected token. If our Keychain helper is blocked, let
        // Claude Code use its own sign-in in this exact profile directory.
        let credentials = match self.load_credentials(profile).await {
            Ok(credentials) if credentials.is_expired() && supports_native_cli_auth(profile) => {
                None
            }
            Ok(credentials) => Some(credentials),
            Err(error)
                if error.kind() == ProviderErrorKind::KeychainAccessFailed
                    && supports_native_cli_auth(profile) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let native_auth = credentials.is_none();
        if native_auth {
            validate_cli_identity(profile, expected_account_id).await?;
        }
        let usage = tokio::task::spawn_blocking(move || {
            collect_usage_from_cli(
                config_dir.as_deref(),
                &profile_id,
                credentials
                    .as_ref()
                    .map(|credentials| credentials.access_token.as_str()),
            )
        })
        .await
        .map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("Claude CLI usage task failed: {err}"),
            )
        })??;
        if native_auth {
            validate_cli_identity(profile, expected_account_id).await?;
        }
        Ok(usage)
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

fn claude_profiles(config: ProviderConfig, home: &Path) -> anyhow::Result<Vec<Arc<ClaudeProfile>>> {
    let default_keychain_account = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    let has_explicit_profiles = !config.profiles.is_empty();
    let configured = if has_explicit_profiles {
        config.profiles
    } else {
        let mut profile = ProviderProfileConfig {
            id: Some("default".to_string()),
            ..ProviderProfileConfig::default()
        };
        settings::update_profile(&mut profile, |settings| {
            settings.keychain_account = Some(default_keychain_account.clone());
            settings.credentials_file = Some(home.join(CLAUDE_CREDENTIALS_FILE));
            settings.cli_enabled = Some(true);
        })?;
        vec![profile]
    };

    configured
        .into_iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, profile)| -> anyhow::Result<_> {
            let settings = settings::profile(&profile)?;
            let id = profile_id(profile.id.as_deref(), index);
            let keychain_account = settings
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
            let config_dir = settings.claude_config_dir.map(expand_home_path);
            let credentials_file_path = settings
                .credentials_file
                .map(expand_home_path)
                .unwrap_or_else(|| match config_dir.as_ref() {
                    Some(root) => root.join(".credentials.json"),
                    None if !has_explicit_profiles || id == "default" => {
                        home.join(CLAUDE_CREDENTIALS_FILE)
                    }
                    // A custom Keychain profile must never borrow the default
                    // account's fallback file. Its file source must be explicit.
                    None => home
                        .join(".usagetracker/profiles/claude")
                        .join(&id)
                        .join(".credentials.json"),
                });
            let identity_file_path = config_dir
                .as_ref()
                .map(|root| root.join(".claude.json"))
                .unwrap_or_else(|| home.join(".claude.json"));
            let mut project_roots = if settings.project_roots.is_empty() {
                config_dir
                    .as_ref()
                    .map(|root| vec![root.join("projects")])
                    .unwrap_or_default()
            } else {
                settings
                    .project_roots
                    .into_iter()
                    .map(expand_home_path)
                    .collect()
            };
            if settings.owns_default_claude_activity {
                project_roots.push(home.join(".config/claude/projects"));
                project_roots.push(home.join(".claude/projects"));
            }
            project_roots.sort();
            project_roots.dedup();
            let keychain_service = settings
                .keychain_service
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .or_else(|| config_dir.as_deref().map(keychain_service_for_config_dir))
                .unwrap_or_else(|| credentials::CLAUDE_KEYCHAIN_SERVICE.to_string());
            Ok(Arc::new(ClaudeProfile {
                id,
                keychain_service,
                keychain_account,
                credentials_file_path,
                config_dir,
                identity_file_path,
                credentials_cache: Mutex::new(None),
                display_name: profile.display_name,
                cli_enabled: settings
                    .cli_enabled
                    .unwrap_or(!has_explicit_profiles || index == 0),
                project_roots,
                cost_cache: Arc::new(StdMutex::new(None)),
            }))
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
    cli_enabled
        && matches!(
            api_error.kind(),
            ProviderErrorKind::Network
                | ProviderErrorKind::ProviderUnavailable
                | ProviderErrorKind::Parse
                | ProviderErrorKind::KeychainAccessFailed
        )
}

fn supports_native_cli_auth(profile: &ClaudeProfile) -> bool {
    let service = match &profile.config_dir {
        Some(dir) => keychain_service_for_config_dir(dir),
        None if profile.id == "default" => credentials::CLAUDE_KEYCHAIN_SERVICE.to_string(),
        None => return false,
    };
    profile.keychain_service == service
        && std::env::var("USER").is_ok_and(|user| profile.keychain_account == user)
}

fn can_use_cached_identity(profile: &ClaudeProfile, credentials: &ClaudeCredentials) -> bool {
    if !supports_native_cli_auth(profile) {
        return false;
    }
    let native_file = match &profile.config_dir {
        Some(root) => root.join(".credentials.json"),
        None => profile
            .identity_file_path
            .parent()
            .unwrap_or(Path::new(""))
            .join(CLAUDE_CREDENTIALS_FILE),
    };
    credentials.uses_native_cli_source(&native_file)
}

/// Cached seat metadata belongs only to the exact account being collected.
fn cached_account_plan(body: &[u8], expected_account_id: &str) -> Option<String> {
    let identity = parse_cached_profile_identity(body).ok()?;
    if identity.account_id != expected_account_id {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let account = value.get("oauthAccount")?;
    ["subscriptionType", "seatTier"]
        .iter()
        .filter_map(|key| account.get(key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

async fn validate_cli_identity(
    profile: &ClaudeProfile,
    expected: &str,
) -> Result<(), ProviderError> {
    let identity = tokio::fs::read(&profile.identity_file_path)
        .await
        .ok()
        .and_then(|body| parse_cached_profile_identity(&body).ok());
    if identity.is_some_and(|identity| identity.account_id == expected) {
        return Ok(());
    }
    Err(ProviderError::new(
        ProviderErrorKind::CredentialsInvalid,
        "Claude CLI profile identity is missing or changed; refresh account discovery",
    ))
}

fn deduplicate_accounts(accounts: &mut Vec<DiscoveredAccount>) -> Vec<AccountDiscoveryFailure> {
    let mut canonical_profiles: BTreeMap<String, String> = BTreeMap::new();
    let mut failures = Vec::new();
    accounts.retain(|account| {
        let profile_id = account.profile_id.as_deref().unwrap_or("unknown");
        if let Some(canonical_profile_id) = canonical_profiles.get(&account.external_account_id) {
            warn!(
                external_account_id = account.external_account_id.as_str(),
                canonical_profile_id = canonical_profile_id.as_str(),
                duplicate_profile_id = profile_id,
                "duplicate Claude account ignored; each account can only be connected once"
            );
            failures.push(AccountDiscoveryFailure {
                profile_id: profile_id.to_string(),
                error: ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    format!(
                        "Claude account {} is already connected by profile {}; duplicate profile {} cannot be collected",
                        account.external_account_id, canonical_profile_id, profile_id
                    ),
                ),
            });
            false
        } else {
            canonical_profiles.insert(account.external_account_id.clone(), profile_id.to_string());
            true
        }
    });
    failures
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

    fn configured_profile_ids(&self) -> Vec<String> {
        self.profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect()
    }

    async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
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
                Err(err) => failures.push(AccountDiscoveryFailure {
                    profile_id: profile.id.clone(),
                    error: err,
                }),
            }
        }

        if !accounts.is_empty() {
            failures.extend(deduplicate_accounts(&mut accounts));
            return Ok(AccountDiscovery::from_parts(accounts, failures));
        }
        Ok(AccountDiscovery::from_parts(accounts, failures))
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<CollectionOutcome, ProviderError> {
        let profile = self.profile_for_account(account).await?;

        let mut warnings = Vec::new();
        let (mut usage, collection_mode) = match self.collect_usage_with_api(&profile).await {
            Ok((usage, _payload)) => (usage, CLAUDE_COLLECTION_MODE.to_string()),
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
                match self
                    .collect_usage_with_cli(&profile, &account.external_account_id)
                    .await
                {
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
                        (cli_usage.usage, CLAUDE_CLI_COLLECTION_MODE.to_string())
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
                        if matches!(
                            cli_err.kind(),
                            ProviderErrorKind::RateLimited | ProviderErrorKind::Unauthorized
                        ) {
                            return Err(cli_err);
                        }
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

        usage.detail.credential_profile = Some(account.external_account_id.clone());
        usage
            .detail
            .extra
            .insert("profile_id".to_string(), json!(profile.id.as_str()));
        if let Some(display_name) = profile.display_name.as_deref() {
            usage
                .detail
                .extra
                .insert("profile_display_name".to_string(), json!(display_name));
        }
        if usage.detail.subscription_type.is_none() {
            if let Ok(credentials) = self.load_credentials(&profile).await {
                usage.detail.subscription_type = credentials.subscription_type.clone();
                usage.detail.extra.insert(
                    "rate_limit_tier".to_string(),
                    json!(credentials.rate_limit_tier),
                );
            }
        }

        if usage
            .detail
            .subscription_type
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            if let Ok(body) = tokio::fs::read(&profile.identity_file_path).await {
                usage.detail.subscription_type =
                    cached_account_plan(&body, &account.external_account_id);
            }
        }

        let mut supplemental = Vec::new();
        match self.collect_local_usage_dataset(&profile).await {
            Ok(Some(dataset)) => supplemental.push(dataset),
            Ok(None) => {}
            Err(error) => warnings.push(error.short_message().to_string()),
        }

        Ok(CollectionOutcome::collected_with_supplemental(
            ProviderCollectionResult {
                usage,
                daily_usage: Vec::new(),
                usage_events: None,
                collection_mode,
                account_email: account.email.clone(),
                warnings,
            },
            supplemental,
        ))
    }

    async fn request_credential_access(
        &self,
        profile_id: Option<&str>,
    ) -> Result<(), ProviderError> {
        let profile = match profile_id {
            Some(id) => self.profiles.iter().find(|profile| profile.id == id),
            None if self.profiles.len() == 1 => self.profiles.first(),
            None => return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Choose a Claude account or pending profile before requesting credential access",
            )),
        }
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "The selected Claude profile is unavailable",
            )
        })?;
        credentials::request_credential_access(
            profile.keychain_service.clone(),
            profile.keychain_account.clone(),
        )
        .await
    }

    async fn invalidate_cached_credentials(
        &self,
        profile_id: Option<&str>,
    ) -> Result<(), ProviderError> {
        let mut matched = false;
        let mut first_error = None;
        for profile in self
            .profiles
            .iter()
            .filter(|profile| profile_id.is_none_or(|profile_id| profile.id == profile_id))
        {
            matched = true;
            let mut cache = profile.credentials_cache.lock().await;
            let service = profile.keychain_service.clone();
            let account = profile.keychain_account.clone();
            let invalidation = tokio::task::spawn_blocking(move || {
                keychain::invalidate_password_cache(&service, &account)
            })
            .await;
            *cache = None;

            let error = match invalidation {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(ProviderError::new(
                    ProviderErrorKind::KeychainAccessFailed,
                    format!("failed to invalidate Claude Keychain cache: {error:?}"),
                )),
                Err(error) => Some(ProviderError::new(
                    ProviderErrorKind::KeychainAccessFailed,
                    format!("Claude Keychain cache invalidation task failed: {error}"),
                )),
            };
            if first_error.is_none() {
                first_error = error;
            }
        }

        if !matched {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                format!(
                    "Claude profile {} no longer exists",
                    profile_id.unwrap_or("unknown")
                ),
            ));
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn collect_local_usage(
        &self,
        account: &Account,
        _current: Option<&UsageSnapshot>,
    ) -> Result<Vec<UsageDataset>, ProviderError> {
        let profile_id = account.profile_id.as_deref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Claude account has no profile identity",
            )
        })?;
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    format!("Claude profile {profile_id} no longer exists"),
                )
            })?;
        Ok(self
            .collect_local_usage_dataset(profile)
            .await?
            .into_iter()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::fs;

    fn configured_profile(
        id: &str,
        config_dir: &str,
        cli_enabled: Option<bool>,
        owns_default_activity: bool,
    ) -> ProviderProfileConfig {
        let mut profile = ProviderProfileConfig {
            id: Some(id.to_string()),
            ..ProviderProfileConfig::default()
        };
        settings::update_profile(&mut profile, |settings| {
            settings.claude_config_dir = Some(PathBuf::from(config_dir));
            settings.cli_enabled = cli_enabled;
            settings.owns_default_claude_activity = owns_default_activity;
        })
        .unwrap();
        profile
    }

    #[tokio::test]
    async fn native_cli_identity_must_match_before_and_after_collection() {
        let root =
            std::env::temp_dir().join(format!("usage-claude-identity-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let config = ProviderConfig {
            profiles: vec![configured_profile(
                "work",
                root.to_str().unwrap(),
                Some(true),
                false,
            )],
            ..Default::default()
        };
        let profiles = claude_profiles(config, &root).unwrap();
        let profile = &profiles[0];
        assert!(
            validate_cli_identity(profile, "986efbc1-2be6-407a-9bcc-2e429b8e358d")
                .await
                .is_err()
        );
        fs::write(
            root.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"986efbc1-2be6-407a-9bcc-2e429b8e358d"}}"#,
        )
        .unwrap();
        assert!(
            validate_cli_identity(profile, "986efbc1-2be6-407a-9bcc-2e429b8e358d")
                .await
                .is_ok()
        );
        fs::write(
            root.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"a9a22a87-46f9-4e1a-b7a2-548b866111b5"}}"#,
        )
        .unwrap();
        assert!(
            validate_cli_identity(profile, "986efbc1-2be6-407a-9bcc-2e429b8e358d")
                .await
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn credential_access_never_guesses_between_profiles() {
        let root = std::env::temp_dir();
        let profiles = claude_profiles(
            ProviderConfig {
                profiles: vec![
                    configured_profile("personal", "/profiles/personal", Some(false), false),
                    configured_profile("work", "/profiles/work", Some(false), false),
                ],
                ..Default::default()
            },
            &root,
        )
        .unwrap();
        let collector = ClaudeCollector {
            profiles,
            api: ClaudeApiClient::new(Duration::from_secs(1), Duration::from_secs(1)).unwrap(),
        };
        for profile in [None, Some("missing")] {
            assert!(collector.request_credential_access(profile).await.is_err());
        }
    }

    #[test]
    fn cached_plan_uses_matching_account_seat_when_subscription_is_missing() {
        let id = "986efbc1-2be6-407a-9bcc-2e429b8e358d";
        let body = serde_json::to_vec(&json!({"oauthAccount": {
            "accountUuid": id, "subscriptionType": " ", "seatTier": "team_standard"
        }}))
        .unwrap();
        assert_eq!(
            cached_account_plan(&body, id).as_deref(),
            Some("team_standard")
        );
        assert_eq!(cached_account_plan(&body, "another-account"), None);
        assert_eq!(cached_account_plan(b"{}", id), None);
        let body = serde_json::to_vec(&json!({"oauthAccount": {
            "accountUuid": id, "subscriptionType": "max", "seatTier": "team_standard"
        }}))
        .unwrap();
        assert_eq!(cached_account_plan(&body, id).as_deref(), Some("max"));
    }

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
        for kind in [
            ProviderErrorKind::CredentialsMissing,
            ProviderErrorKind::CredentialsInvalid,
            ProviderErrorKind::Unauthorized,
        ] {
            assert!(!should_use_cli_fallback(
                true,
                &ProviderError::new(kind, "credential failure")
            ));
        }
        assert!(should_use_cli_fallback(
            true,
            &ProviderError::new(ProviderErrorKind::KeychainAccessFailed, "permission needed")
        ));
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
                    configured_profile("personal", "/profiles/personal", None, false),
                    configured_profile("work", "/profiles/work", Some(true), false),
                ],
                ..ProviderConfig::default()
            },
            home,
        )
        .unwrap();

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
            profiles[0].credentials_file_path,
            PathBuf::from("/profiles/personal/.credentials.json")
        );
        assert_eq!(
            profiles[1].credentials_file_path,
            PathBuf::from("/profiles/work/.credentials.json")
        );
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

        let failures = deduplicate_accounts(&mut accounts);

        assert_eq!(accounts.len(), 2);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].profile_id, "second");
        assert_eq!(
            failures[0].error.kind(),
            ProviderErrorKind::CredentialsInvalid
        );
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

        assert_eq!(
            personal_usage.detail.cost.as_ref().unwrap().total_tokens,
            Some(11)
        );
        assert_eq!(
            work_usage.detail.cost.as_ref().unwrap().total_tokens,
            Some(22)
        );
        assert_eq!(
            personal_usage.detail.cost.as_ref().unwrap().extra["files_scanned"],
            1
        );
        assert_eq!(
            work_usage.detail.cost.as_ref().unwrap().extra["files_scanned"],
            1
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn default_activity_owner_scans_legacy_and_managed_roots() {
        let home = Path::new("/Users/test");
        let profiles = claude_profiles(
            ProviderConfig {
                enabled: true,
                profiles: vec![configured_profile(
                    "personal",
                    "/profiles/personal",
                    None,
                    true,
                )],
                ..ProviderConfig::default()
            },
            home,
        )
        .unwrap();

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
            detail: SnapshotDetail::default(),
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
