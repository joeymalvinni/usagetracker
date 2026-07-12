//! Grok collection orchestration: official CLI billing RPC, then grok.com.

mod auth;
mod billing;
mod cookies;
mod local_sessions;
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
use usage_core::ProviderId;

use crate::{
    config::ProviderConfig,
    providers::{
        DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
        ProviderErrorKind, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    },
};

use billing::{BillingData, BillingSource};
use cookies::CookieCandidate;
use strategy::SourceMode;

pub const PROVIDER_ID: &str = "grok";
pub(crate) use rpc::find_grok_binary;
const DEFAULT_EXTERNAL_ACCOUNT_ID: &str = "grok_default";
const DEFAULT_PROFILE_ID: &str = "default";
const BROWSER_IMPORT_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct BrowserSessionCache {
    imported_at: Option<Instant>,
    candidates: Vec<CookieCandidate>,
}

pub struct GrokCollector {
    config: ProviderConfig,
    client: reqwest::Client,
    capture_raw_payloads: bool,
    source_mode: SourceMode,
    discovered_browser_sessions: Mutex<BrowserSessionCache>,
}

impl GrokCollector {
    pub fn new(config: ProviderConfig, capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let source_mode = SourceMode::parse(config.source_mode.as_deref())
            .map_err(|error| anyhow::anyhow!(error.short_message().to_string()))?;
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent(concat!("UsageTracker/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            config,
            client,
            capture_raw_payloads,
            source_mode,
            discovered_browser_sessions: Mutex::new(BrowserSessionCache::default()),
        })
    }

    fn discovered_account(credentials: Option<&auth::GrokCredentials>) -> DiscoveredAccount {
        DiscoveredAccount {
            external_account_id: credentials
                .map(auth::GrokCredentials::external_account_id)
                .unwrap_or_else(|| DEFAULT_EXTERNAL_ACCOUNT_ID.to_string()),
            display_name: credentials.and_then(|value| value.display_name.clone()),
            email: credentials.and_then(|value| value.email.clone()),
            profile_id: Some(DEFAULT_PROFILE_ID.to_string()),
        }
    }

    async fn cli_billing(&self) -> Result<BillingData, ProviderError> {
        let binary = rpc::find_grok_binary().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "Grok CLI is not installed",
            )
        })?;
        let value = tokio::task::spawn_blocking(move || rpc::fetch_billing(&binary))
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("Grok RPC task failed: {err}"),
                )
            })??;
        billing::from_rpc(&value)
    }

    fn initial_cookie_candidates(&self) -> Vec<CookieCandidate> {
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
        cookies::cached_candidate().into_iter().collect()
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
        credentials: Option<&auth::GrokCredentials>,
    ) -> Result<(BillingData, String), ProviderError> {
        let bearer = credentials
            .filter(|value| !value.is_expired())
            .map(|value| value.access_token.as_str());
        let mut candidates = self.initial_cookie_candidates();
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
                            cookies::store(candidate);
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
            cookies::clear_cache();
            for candidate in self.import_browser_candidates().await.unwrap_or_default() {
                for auth in web_auth_attempts(bearer) {
                    match web::fetch(&self.client, auth, Some(&candidate.header)).await {
                        Ok(data) => {
                            cookies::store(&candidate);
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

    async fn collection_result(
        &self,
        data: BillingData,
        source: BillingSource,
        credential_source: Option<String>,
        credentials: Option<&auth::GrokCredentials>,
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
        if let Ok(Ok(summary)) = tokio::task::spawn_blocking(local_sessions::scan_default).await {
            usage.metadata["local_sessions"] = summary.metadata();
        }
        ProviderCollectionResult {
            raw_payload: self
                .capture_raw_payloads
                .then(|| normalized_raw(&data, collection_mode)),
            usage,
            daily_usage: Vec::new(),
            collection_mode: collection_mode.to_string(),
            account_email: credentials.and_then(|value| value.email.clone()),
            warnings,
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

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        let credentials_error = match auth::load_credentials() {
            Ok(credentials) => return Ok(vec![Self::discovered_account(Some(&credentials))]),
            Err(error) => error,
        };
        if (self.source_mode.uses_web()
            && (cookies::manual_candidate(&self.config).is_some()
                || cookies::cached_candidate().is_some()))
            || (self.source_mode.uses_cli()
                && std::env::var_os("XAI_API_KEY").is_some()
                && rpc::find_grok_binary().is_some())
        {
            return Ok(vec![Self::discovered_account(None)]);
        }
        if self.source_mode.uses_web() && cookies::manual_is_configured(&self.config) {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "the configured Grok cookie header does not contain sso or sso-rw",
            ));
        }
        if self.source_mode.uses_web() && self.config.enabled {
            if let Ok(sessions) = self.import_browser_candidates().await {
                if !sessions.is_empty() {
                    return Ok(vec![Self::discovered_account(None)]);
                }
            }
        }
        if auth::auth_file_exists() {
            return Err(credentials_error);
        }
        Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            match self.source_mode {
                SourceMode::Cli => "Grok CLI is not connected; run `grok login`",
                SourceMode::Web => {
                    "Grok web billing is not connected; run `grok login` or sign in at grok.com in Chrome"
                }
                SourceMode::Auto => {
                    "Grok is not connected; run `grok login` or sign in at grok.com in Chrome"
                }
            },
        ))
    }

    async fn collect_usage(
        &self,
        _account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let credentials = auth::load_credentials().ok();
        let mut cli_error = None;
        if self.source_mode.uses_cli() {
            match self.cli_billing().await {
                Ok(data) => {
                    return Ok(self
                        .collection_result(
                            data,
                            BillingSource::CliRpc,
                            None,
                            credentials.as_ref(),
                            Vec::new(),
                        )
                        .await)
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
        match self.collect_web(credentials.as_ref()).await {
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
                Ok(self
                    .collection_result(
                        data,
                        BillingSource::GrokWeb,
                        Some(cookie_source),
                        credentials.as_ref(),
                        warnings,
                    )
                    .await)
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

fn normalized_raw(data: &BillingData, source: &str) -> serde_json::Value {
    json!({
        "source": source,
        "used_percent": data.used_percent,
        "period_start": data.period_start,
        "resets_at": data.resets_at,
        "used_usd": data.used_usd,
        "limit_usd": data.limit_usd,
        "on_demand_used_usd": data.on_demand_used_usd,
        "on_demand_limit_usd": data.on_demand_limit_usd,
    })
}

pub(crate) async fn clear_cached_cookie_cache() -> anyhow::Result<()> {
    tokio::task::spawn_blocking(cookies::clear_cache).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
