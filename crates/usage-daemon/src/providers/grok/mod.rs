//! Grok collection orchestration: official CLI billing RPC, then grok.com.

mod auth;
mod billing;
mod cookies;
mod local_sessions;
mod profile;
pub(crate) mod profile_service;
mod rpc;
mod strategy;
mod web;

use std::{
    collections::BTreeSet,
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::redirect::Policy;
use serde_json::json;
use usage_core::{
    Account, ProviderId, UsageDataCompleteness, UsageDataQuality, UsageDataScope, UsageDataSource,
    UsageSnapshot,
};

use crate::{
    config::ProviderConfig,
    providers::{
        AccountDiscovery, AccountDiscoveryFailure, CollectionOutcome, DiscoveredAccount,
        ProviderCollectionResult, ProviderCollector, ProviderError, ProviderErrorKind,
        ProviderUsage, UsageDataset, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    },
};

use billing::{BillingData, BillingSource};
use cookies::CookieCandidate;
use profile::{deduplicate_accounts, GrokProfile};
use strategy::SourceMode;

pub const PROVIDER_ID: &str = "grok";

pub(crate) mod adapter;
pub(crate) mod settings;
pub(crate) use profile::{
    default_home as default_grok_home, normalized_id as normalized_profile_id, DEFAULT_PROFILE_ID,
};
pub(crate) use rpc::find_grok_binary;
const DEFAULT_EXTERNAL_ACCOUNT_ID: &str = "grok_default";
const BROWSER_IMPORT_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct BrowserSessionCache {
    imported_at: Option<Instant>,
    candidates: Vec<CookieCandidate>,
}

pub struct GrokCollector {
    config: ProviderConfig,
    profiles: Vec<GrokProfile>,
    client: reqwest::Client,
    source_mode: SourceMode,
    discovered_browser_sessions: Mutex<BrowserSessionCache>,
}

impl GrokCollector {
    pub fn new(config: ProviderConfig) -> anyhow::Result<Self> {
        let provider_settings = settings::provider(&config)?;
        let source_mode = SourceMode::parse(provider_settings.source_mode.as_deref())
            .map_err(|error| anyhow::anyhow!(error.short_message().to_string()))?;
        let profiles = profile::resolve(&config)?;
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent(concat!("UsageTracker/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            config,
            profiles,
            client,
            source_mode,
            discovered_browser_sessions: Mutex::new(BrowserSessionCache::default()),
        })
    }

    fn discovered_account(
        profile: &GrokProfile,
        credentials: Option<&auth::GrokCredentials>,
    ) -> DiscoveredAccount {
        DiscoveredAccount {
            external_account_id: credentials
                .map(auth::GrokCredentials::external_account_id)
                .unwrap_or_else(|| DEFAULT_EXTERNAL_ACCOUNT_ID.to_string()),
            display_name: profile
                .display_name
                .clone()
                .or_else(|| credentials.and_then(|value| value.display_name.clone())),
            email: credentials.and_then(|value| value.email.clone()),
            profile_id: Some(profile.id.clone()),
        }
    }

    fn profile_for_account(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<&GrokProfile, ProviderError> {
        let profile_id = account.profile_id.as_deref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Grok account is missing its profile identity",
            )
        })?;
        self.profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    format!("Grok profile {profile_id} no longer exists"),
                )
            })
    }

    async fn cli_billing(&self, profile: &GrokProfile) -> Result<BillingData, ProviderError> {
        let binary = rpc::find_grok_binary().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "Grok CLI is not installed",
            )
        })?;
        let grok_home = profile.grok_home.clone();
        let value = tokio::task::spawn_blocking(move || rpc::fetch_billing(&binary, &grok_home))
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("Grok RPC task failed: {err}"),
                )
            })??;
        billing::from_rpc(&value)
    }

    async fn initial_cookie_candidates(&self) -> Vec<CookieCandidate> {
        if let Some(manual) = cookies::manual_candidate(&self.config) {
            return vec![manual];
        }
        let discovered = self
            .discovered_browser_sessions
            .lock()
            .ok()
            .filter(|cache| {
                cache
                    .imported_at
                    .is_some_and(|at| at.elapsed() <= BROWSER_IMPORT_CACHE_TTL)
            })
            .map(|cache| cache.candidates.clone())
            .unwrap_or_default();
        if !discovered.is_empty() {
            return discovered;
        }
        cookies::cached_candidate().await.into_iter().collect()
    }

    async fn import_browser_candidates(&self) -> Result<Vec<CookieCandidate>, ProviderError> {
        let candidates = tokio::task::spawn_blocking(cookies::import_browser_candidates)
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("Grok browser cookie task failed: {err}"),
                )
            })??;
        if let Ok(mut cache) = self.discovered_browser_sessions.lock() {
            cache.imported_at = Some(Instant::now());
            cache.candidates = candidates.clone();
        }
        Ok(candidates)
    }

    async fn collect_web(
        &self,
        profile: &GrokProfile,
        credentials: Option<&auth::GrokCredentials>,
    ) -> Result<(BillingData, String), ProviderError> {
        let bearer = credentials
            .filter(|value| !value.is_expired())
            .map(|value| value.access_token.as_str());
        if !profile.allows_legacy_browser_auth {
            let bearer = bearer.ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsMissing,
                    format!("Grok profile {} requires its own CLI login", profile.id),
                )
            })?;
            return web::fetch(&self.client, Some(bearer), None)
                .await
                .map(|data| (data, "grok_profile_auth_token".to_string()));
        }
        let mut candidates = self.initial_cookie_candidates().await;
        if candidates.is_empty() && cookies::manual_is_configured(&self.config) {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "the configured Grok cookie header does not contain sso or sso-rw",
            ));
        }
        if candidates.is_empty() && self.config.enabled {
            candidates = self.import_browser_candidates().await.unwrap_or_default();
        }

        let mut last_error = None;
        let mut seen = BTreeSet::new();
        for candidate in &candidates {
            if !seen.insert(candidate.header.as_str()) {
                continue;
            }
            for auth in web_auth_attempts(bearer) {
                match web::fetch(&self.client, auth, Some(&candidate.header)).await {
                    Ok(data) => {
                        if candidate.browser_imported {
                            cookies::store(candidate).await;
                        }
                        return Ok((data, candidate.source.clone()));
                    }
                    Err(err) if err.kind() == ProviderErrorKind::RateLimited => return Err(err),
                    Err(err) => last_error = Some(err),
                }
            }
        }

        if let Some(bearer) = bearer {
            match web::fetch(&self.client, Some(bearer), None).await {
                Ok(data) => return Ok((data, "grok_auth_token".to_string())),
                Err(err) if err.kind() == ProviderErrorKind::RateLimited => return Err(err),
                Err(err) => last_error = Some(err),
            }
        }

        // A cached browser session may be stale. Clear it and try every current
        // browser profile once; explicit manual cookies are never bypassed.
        if !cookies::manual_is_configured(&self.config)
            && candidates
                .iter()
                .any(|candidate| candidate.source == "keychain_cache")
        {
            cookies::clear_cache().await;
            for candidate in self.import_browser_candidates().await.unwrap_or_default() {
                for auth in web_auth_attempts(bearer) {
                    match web::fetch(&self.client, auth, Some(&candidate.header)).await {
                        Ok(data) => {
                            cookies::store(&candidate).await;
                            return Ok((data, candidate.source));
                        }
                        Err(err) if err.kind() == ProviderErrorKind::RateLimited => {
                            return Err(err)
                        }
                        Err(err) => last_error = Some(err),
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "Grok billing requires `grok login` or a signed-in grok.com browser session",
            )
        }))
    }

    fn collection_result(
        &self,
        data: BillingData,
        source: BillingSource,
        credential_source: Option<String>,
        credentials: Option<&auth::GrokCredentials>,
        profile: &GrokProfile,
        warnings: Vec<String>,
    ) -> ProviderCollectionResult {
        let collection_mode = source.collection_mode();
        let mut usage = billing::to_provider_usage(&data, source);
        if let Some(source) = credential_source {
            usage.metadata["credential_source"] = json!(source);
        }
        if let Some(credentials) = credentials {
            usage.metadata["identity"] = credentials.metadata();
        }
        usage.metadata["profile_id"] = json!(profile.id.as_str());
        if let Some(display_name) = profile.display_name.as_deref() {
            usage.metadata["profile_display_name"] = json!(display_name);
        }
        ProviderCollectionResult {
            usage,
            daily_usage: Vec::new(),
            collection_mode: collection_mode.to_string(),
            account_email: credentials.and_then(|value| value.email.clone()),
            warnings,
        }
    }

    async fn collect_local_usage_dataset(
        &self,
        profile: &GrokProfile,
    ) -> Result<UsageDataset, ProviderError> {
        let grok_home = profile.grok_home.clone();
        let summary = tokio::task::spawn_blocking(move || local_sessions::scan_home(&grok_home))
            .await
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("Grok local session scan task failed: {error}"),
                )
            })??;
        Ok(UsageDataset::supplemental_named(
            "grok_local_sessions",
            ProviderCollectionResult {
                usage: ProviderUsage {
                    provider_id: ProviderId::new(PROVIDER_ID),
                    collected_at: chrono::Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({"local_sessions": summary.metadata()}),
                },
                daily_usage: Vec::new(),
                collection_mode: "grok_local_sessions".to_string(),
                account_email: None,
                warnings: Vec::new(),
            },
            UsageDataSource::LocalLogs,
            UsageDataScope::ThisDevice,
            UsageDataQuality::Observed,
            UsageDataCompleteness::Partial,
        ))
    }

    async fn collected_with_local_usage(
        &self,
        mut collection: ProviderCollectionResult,
        profile: &GrokProfile,
    ) -> CollectionOutcome {
        match self.collect_local_usage_dataset(profile).await {
            Ok(dataset) => {
                CollectionOutcome::collected_with_supplemental(collection, vec![dataset])
            }
            Err(error) => {
                collection.warnings.push(error.short_message().to_string());
                CollectionOutcome::collected(collection)
            }
        }
    }
}

fn web_auth_attempts(bearer: Option<&str>) -> impl Iterator<Item = Option<&str>> {
    std::iter::once(bearer).chain(bearer.map(|_| None))
}

#[async_trait]
impl ProviderCollector for GrokCollector {
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
                "no enabled Grok profiles are configured",
            ));
        }

        let mut accounts = Vec::new();
        let mut failures = Vec::new();
        for profile in &self.profiles {
            match auth::load_credentials(&profile.grok_home) {
                Ok(credentials) => {
                    accounts.push(Self::discovered_account(profile, Some(&credentials)))
                }
                Err(error) => failures.push(AccountDiscoveryFailure {
                    profile_id: profile.id.clone(),
                    error,
                }),
            }
        }

        if let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.allows_legacy_browser_auth)
        {
            let already_discovered = accounts
                .iter()
                .any(|account| account.profile_id.as_deref() == Some(profile.id.as_str()));
            if !already_discovered {
                let configured_auth = self.source_mode.uses_web()
                    && (cookies::manual_candidate(&self.config).is_some()
                        || cookies::cached_candidate().await.is_some());
                let api_key_auth = self.source_mode.uses_cli()
                    && std::env::var_os("XAI_API_KEY").is_some()
                    && rpc::find_grok_binary().is_some();
                if configured_auth || api_key_auth {
                    accounts.push(Self::discovered_account(profile, None));
                } else if self.source_mode.uses_web() && cookies::manual_is_configured(&self.config)
                {
                    failures.push(AccountDiscoveryFailure {
                        profile_id: profile.id.clone(),
                        error: ProviderError::new(
                            ProviderErrorKind::CredentialsInvalid,
                            "the configured Grok cookie header does not contain sso or sso-rw",
                        ),
                    });
                } else if self.source_mode.uses_web() && self.config.enabled {
                    if let Ok(sessions) = self.import_browser_candidates().await {
                        if !sessions.is_empty() {
                            accounts.push(Self::discovered_account(profile, None));
                        }
                    }
                }
            }
        }

        if !accounts.is_empty() {
            deduplicate_accounts(&mut accounts);
            failures.retain(|failure| {
                !accounts.iter().any(|account| {
                    account.profile_id.as_deref() == Some(failure.profile_id.as_str())
                })
            });
        }
        deduplicate_discovery_failures(&mut failures);
        Ok(AccountDiscovery::from_parts(accounts, failures))
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<CollectionOutcome, ProviderError> {
        let profile = self.profile_for_account(account)?;
        let credentials = auth::load_credentials(&profile.grok_home).ok();
        if let Some(credentials) = credentials.as_ref() {
            let discovered_identity = credentials.external_account_id();
            if account.external_account_id != DEFAULT_EXTERNAL_ACCOUNT_ID
                && account.external_account_id != discovered_identity
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    format!(
                        "Grok profile {} is signed in to a different account",
                        profile.id
                    ),
                ));
            }
        }
        let mut cli_error = None;
        if self.source_mode.uses_cli() {
            match self.cli_billing(profile).await {
                Ok(data) => {
                    let collection = self.collection_result(
                        data,
                        BillingSource::CliRpc,
                        None,
                        credentials.as_ref(),
                        profile,
                        Vec::new(),
                    );
                    return Ok(self.collected_with_local_usage(collection, profile).await);
                }
                Err(error)
                    if self.source_mode.permits_fallback() && should_fallback_after_cli(&error) =>
                {
                    cli_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        if !self.source_mode.uses_web() {
            return Err(cli_error.unwrap_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    "Grok has no enabled collection source",
                )
            }));
        }
        match self.collect_web(profile, credentials.as_ref()).await {
            Ok((data, cookie_source)) => {
                let warnings = cli_error
                    .as_ref()
                    .map(|error| {
                        vec![format!(
                            "Grok CLI billing was unavailable; used grok.com: {}",
                            error.short_message()
                        )]
                    })
                    .unwrap_or_default();
                let collection = self.collection_result(
                    data,
                    BillingSource::GrokWeb,
                    Some(cookie_source),
                    credentials.as_ref(),
                    profile,
                    warnings,
                );
                Ok(self.collected_with_local_usage(collection, profile).await)
            }
            Err(web_error) if web_error.kind() == ProviderErrorKind::RateLimited => Err(web_error),
            Err(web_error) => match cli_error {
                Some(cli_error) => Err(ProviderError::new(
                    preferred_error_kind(&cli_error, &web_error),
                    format!(
                        "Grok CLI billing failed ({}); grok.com fallback failed ({})",
                        cli_error.short_message(),
                        web_error.short_message()
                    ),
                )),
                None => Err(web_error),
            },
        }
    }

    async fn collect_local_usage(
        &self,
        account: &Account,
        _current: Option<&UsageSnapshot>,
    ) -> Result<Vec<UsageDataset>, ProviderError> {
        let profile_id = account.profile_id.as_deref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Grok account is missing its profile identity",
            )
        })?;
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    format!("Grok profile {profile_id} no longer exists"),
                )
            })?;
        Ok(vec![self.collect_local_usage_dataset(profile).await?])
    }
}

fn deduplicate_discovery_failures(failures: &mut Vec<AccountDiscoveryFailure>) {
    let mut deduplicated: Vec<AccountDiscoveryFailure> = Vec::with_capacity(failures.len());
    for failure in failures.drain(..) {
        if let Some(existing) = deduplicated
            .iter_mut()
            .find(|existing| existing.profile_id == failure.profile_id)
        {
            if existing.error.kind() == ProviderErrorKind::CredentialsMissing
                && failure.error.kind() != ProviderErrorKind::CredentialsMissing
            {
                *existing = failure;
            }
        } else {
            deduplicated.push(failure);
        }
    }
    *failures = deduplicated;
}

fn preferred_error_kind(cli: &ProviderError, web: &ProviderError) -> ProviderErrorKind {
    for kind in [
        ProviderErrorKind::Unauthorized,
        ProviderErrorKind::CredentialsInvalid,
        ProviderErrorKind::CredentialsMissing,
        ProviderErrorKind::Parse,
    ] {
        if cli.kind() == kind || web.kind() == kind {
            return kind;
        }
    }
    web.kind()
}

fn should_fallback_after_cli(error: &ProviderError) -> bool {
    error.kind() != ProviderErrorKind::RateLimited
}

pub(crate) async fn clear_cached_cookie_cache() -> anyhow::Result<()> {
    cookies::clear_cache().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    use crate::config::ProviderProfileConfig;
    use crate::providers::AuthoritativeOutcome;

    #[test]
    fn duplicate_profile_failures_keep_the_more_specific_error() {
        let mut failures = vec![
            AccountDiscoveryFailure {
                profile_id: "default".to_string(),
                error: ProviderError::new(ProviderErrorKind::CredentialsMissing, "missing"),
            },
            AccountDiscoveryFailure {
                profile_id: "default".to_string(),
                error: ProviderError::new(ProviderErrorKind::CredentialsInvalid, "invalid"),
            },
        ];

        deduplicate_discovery_failures(&mut failures);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].profile_id, "default");
        assert_eq!(
            failures[0].error.kind(),
            ProviderErrorKind::CredentialsInvalid
        );
    }

    fn write_auth(home: &std::path::Path, user_id: &str, email: &str) {
        fs::create_dir_all(home).unwrap();
        let payload = serde_json::json!({
            "https://auth.x.ai::client": {
                "key": format!("token-{user_id}"),
                "user_id": user_id,
                "email": email
            }
        });
        fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
    }

    fn profile(id: &str, home: PathBuf) -> ProviderProfileConfig {
        let mut profile = ProviderProfileConfig {
            id: Some(id.to_string()),
            display_name: Some(id.to_string()),
            ..ProviderProfileConfig::default()
        };
        settings::update_profile(&mut profile, |settings| settings.grok_home = Some(home)).unwrap();
        profile
    }

    fn cli_config(profiles: Vec<ProviderProfileConfig>) -> ProviderConfig {
        let mut config = ProviderConfig {
            profiles,
            ..ProviderConfig::default()
        };
        settings::update_provider(&mut config, |settings| {
            settings.source_mode = Some("cli".to_string());
        })
        .unwrap();
        config
    }

    #[test]
    fn rate_limits_never_switch_to_browser_credentials() {
        let rate_limit = ProviderError::new(ProviderErrorKind::RateLimited, "slow down");
        let unavailable =
            ProviderError::new(ProviderErrorKind::ProviderUnavailable, "method not found");
        assert!(!should_fallback_after_cli(&rate_limit));
        assert!(should_fallback_after_cli(&unavailable));
    }

    #[test]
    fn web_auth_tries_combined_credentials_before_cookie_only() {
        assert_eq!(
            web_auth_attempts(Some("token")).collect::<Vec<_>>(),
            vec![Some("token"), None]
        );
        assert_eq!(web_auth_attempts(None).collect::<Vec<_>>(), vec![None]);
    }

    #[tokio::test]
    async fn full_collection_keeps_local_sessions_in_a_supplemental_dataset() {
        let root = std::env::temp_dir().join(format!("grok-local-{}", uuid::Uuid::new_v4()));
        let collector =
            GrokCollector::new(cli_config(vec![profile("default", root.clone())])).unwrap();
        let profile = collector.profiles[0].clone();
        let outcome = collector
            .collected_with_local_usage(
                ProviderCollectionResult {
                    usage: ProviderUsage {
                        provider_id: ProviderId::new(PROVIDER_ID),
                        collected_at: chrono::Utc::now(),
                        windows: Vec::new(),
                        metadata: json!({"remote": true}),
                    },
                    daily_usage: Vec::new(),
                    collection_mode: "grok_cli_billing_rpc".to_string(),
                    account_email: None,
                    warnings: Vec::new(),
                },
                &profile,
            )
            .await;

        let AuthoritativeOutcome::Collected(authoritative) = outcome.authoritative else {
            panic!("full collection should remain authoritative");
        };
        assert_eq!(authoritative.collection.usage.metadata["remote"], true);
        assert!(authoritative
            .collection
            .usage
            .metadata
            .get("local_sessions")
            .is_none());
        assert_eq!(outcome.supplemental.len(), 1);
        assert_eq!(outcome.supplemental[0].source_id, "grok_local_sessions");
        assert!(!outcome.supplemental[0].authoritative);
        assert_eq!(
            outcome.supplemental[0].provenance.source,
            UsageDataSource::LocalLogs
        );
    }

    #[tokio::test]
    async fn discovers_distinct_accounts_from_isolated_grok_homes() {
        let root = std::env::temp_dir().join(format!("grok-profiles-{}", uuid::Uuid::new_v4()));
        let personal = root.join("personal");
        let work = root.join("work");
        write_auth(&personal, "user-personal", "personal@example.com");
        write_auth(&work, "user-work", "work@example.com");
        let collector = GrokCollector::new(cli_config(vec![
            profile("personal", personal),
            profile("work", work),
        ]))
        .unwrap();

        let discovery = collector.discover_accounts().await.unwrap();
        let (accounts, _) = discovery.into_parts();

        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].external_account_id, "user-personal");
        assert_eq!(accounts[0].profile_id.as_deref(), Some("personal"));
        assert_eq!(accounts[1].external_account_id, "user-work");
        assert_eq!(accounts[1].profile_id.as_deref(), Some("work"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn duplicate_grok_identity_keeps_the_first_profile() {
        let root = std::env::temp_dir().join(format!("grok-profiles-{}", uuid::Uuid::new_v4()));
        let first = root.join("first");
        let duplicate = root.join("duplicate");
        write_auth(&first, "same-user", "same@example.com");
        write_auth(&duplicate, "same-user", "same@example.com");
        let collector = GrokCollector::new(cli_config(vec![
            profile("first", first),
            profile("duplicate", duplicate),
        ]))
        .unwrap();

        let discovery = collector.discover_accounts().await.unwrap();
        let (accounts, _) = discovery.into_parts();

        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].profile_id.as_deref(), Some("first"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reports_a_failed_profile_alongside_healthy_accounts() {
        let root = std::env::temp_dir().join(format!("grok-profiles-{}", uuid::Uuid::new_v4()));
        let healthy = root.join("healthy");
        write_auth(&healthy, "healthy-user", "healthy@example.com");
        let collector = GrokCollector::new(cli_config(vec![
            profile("healthy", healthy),
            profile("broken", root.join("broken")),
        ]))
        .unwrap();

        let discovery = collector.discover_accounts().await.unwrap();
        let (accounts, failures) = discovery.into_parts();

        assert_eq!(accounts.len(), 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].profile_id, "broken");
        assert_eq!(
            failures[0].error.kind(),
            ProviderErrorKind::CredentialsMissing
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn returns_one_failure_for_each_grok_profile_when_all_discovery_fails() {
        let root = std::env::temp_dir().join(format!("grok-profiles-{}", uuid::Uuid::new_v4()));
        let collector = GrokCollector::new(cli_config(vec![
            profile("personal", root.join("personal")),
            profile("work", root.join("work")),
        ]))
        .unwrap();

        let discovery = collector.discover_accounts().await.unwrap();
        let (accounts, failures) = discovery.into_parts();

        assert!(accounts.is_empty());
        assert_eq!(
            failures
                .iter()
                .map(|failure| failure.profile_id.as_str())
                .collect::<Vec<_>>(),
            ["personal", "work"]
        );
    }

    #[test]
    fn rejects_duplicate_canonical_profile_ids() {
        let root = std::env::temp_dir().join(format!("grok-profiles-{}", uuid::Uuid::new_v4()));
        let error = GrokCollector::new(ProviderConfig {
            profiles: vec![
                profile("work", root.join("first")),
                profile(" work ", root.join("second")),
            ],
            ..ProviderConfig::default()
        })
        .err()
        .unwrap();

        assert!(error
            .to_string()
            .contains("duplicate Grok profile id: work"));
    }

    #[test]
    fn managed_profiles_cannot_consume_global_browser_credentials() {
        let root = std::env::temp_dir().join(format!("grok-profiles-{}", uuid::Uuid::new_v4()));
        let profiles = profile::resolve(&ProviderConfig {
            profiles: vec![profile("work", root.join("work"))],
            ..ProviderConfig::default()
        })
        .unwrap();

        assert_eq!(profiles.len(), 1);
        assert!(!profiles[0].allows_legacy_browser_auth);
    }

    #[test]
    fn non_default_profile_requires_an_explicit_home() {
        let error = GrokCollector::new(ProviderConfig {
            profiles: vec![ProviderProfileConfig {
                id: Some("work".to_string()),
                ..ProviderProfileConfig::default()
            }],
            ..ProviderConfig::default()
        })
        .err()
        .unwrap();

        assert!(error.to_string().contains("missing its home directory"));
    }
}
