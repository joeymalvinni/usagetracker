//! Cursor web-session usage collection.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use reqwest::redirect::Policy;
use usage_core::ProviderId;

use crate::{
    config::ProviderConfig,
    providers::{
        AccountDiscovery, CollectionOutcome, DiscoveredAccount, ProviderCollector, ProviderError,
        ProviderErrorKind, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    },
};

use self::{
    auth::{load_session_candidates, SessionCredential, SessionSource},
    client::CursorClient,
    model::{normalize_cursor_fetch, CursorUserInfo},
};

pub const CURSOR_PROVIDER_ID: &str = "cursor";

pub(crate) mod adapter;
mod auth;
mod client;
mod events;
mod model;
mod number;
pub(crate) mod settings;

#[derive(Clone)]
pub struct CursorCollector {
    client: CursorClient,
    cache: Arc<SessionCache>,
}

impl CursorCollector {
    pub fn new(config: ProviderConfig) -> anyhow::Result<Self> {
        let _settings = settings::provider(&config)?;
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent("UsageTracker/0.1 CursorUsage")
            .redirect(Policy::custom(|attempt| {
                let same_https_host = attempt
                    .previous()
                    .last()
                    .and_then(|url| url.host_str())
                    .zip(attempt.url().host_str())
                    .is_some_and(|(source, destination)| {
                        source.eq_ignore_ascii_case(destination)
                            && attempt.url().scheme().eq_ignore_ascii_case("https")
                    });
                if same_https_host {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        Ok(Self {
            client: CursorClient::new(client),
            cache: Arc::new(SessionCache::default()),
        })
    }

    async fn discover_and_cache(
        &self,
    ) -> Result<Vec<(DiscoveredAccount, CachedSession)>, ProviderError> {
        let candidates = tokio::task::spawn_blocking(load_session_candidates)
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("Cursor credential discovery task failed: {err}"),
                )
            })??;
        let manual_only = candidates
            .first()
            .is_some_and(|candidate| candidate.source == SessionSource::Manual);
        let mut discovered = BTreeMap::<String, (DiscoveredAccount, SessionCredential)>::new();
        let mut first_error = None;
        for candidate in candidates {
            match self.client.fetch_identity(&candidate.cookie_header).await {
                Ok(identity) => {
                    let Some(external_account_id) = identity
                        .stable_id()
                        .map(str::to_string)
                        .or_else(|| candidate.account_hint.clone())
                    else {
                        first_error.get_or_insert_with(|| {
                            ProviderError::new(
                                ProviderErrorKind::Parse,
                                "Cursor account identity did not include a stable user ID",
                            )
                        });
                        continue;
                    };
                    discovered
                        .entry(external_account_id.clone())
                        .or_insert_with(|| {
                            (
                                discovered_account(&external_account_id, &identity),
                                candidate,
                            )
                        });
                }
                Err(error) if manual_only => return Err(error),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if discovered.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsMissing,
                    "no validated Cursor account sessions were found",
                )
            }));
        }

        Ok(discovered
            .into_iter()
            .map(|(account_id, (account, credential))| {
                let cached = self.cache.store(account_id, credential);
                (account, cached)
            })
            .collect())
    }

    async fn session_for_account(
        &self,
        external_account_id: &str,
    ) -> Result<CachedSession, ProviderError> {
        if let Some(session) = self.cache.get(external_account_id) {
            if session.credential.source != SessionSource::CursorApp {
                return Ok(session);
            }
            // Cursor owns refresh and account switching for its desktop token.
            // Re-read and re-validate app auth at collection time instead of
            // turning the daemon cache into a second credential store.
            let refreshed = self
                .discover_and_cache()
                .await?
                .into_iter()
                .find(|(account, _)| account.external_account_id == external_account_id)
                .map(|(_, session)| session);
            if let Some(refreshed) = refreshed {
                return Ok(refreshed);
            }
            self.cache
                .remove_if_generation(external_account_id, session.generation);
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                format!("Cursor.app is no longer signed in as account {external_account_id}"),
            ));
        }
        self.discover_and_cache()
            .await?
            .into_iter()
            .find(|(account, _)| account.external_account_id == external_account_id)
            .map(|(_, session)| session)
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsMissing,
                    format!("no current Cursor session matched account {external_account_id}"),
                )
            })
    }
}

#[async_trait]
impl ProviderCollector for CursorCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(CURSOR_PROVIDER_ID)
    }

    fn configured_profile_ids(&self) -> Vec<String> {
        Vec::new()
    }

    async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
        Ok(self
            .discover_and_cache()
            .await?
            .into_iter()
            .map(|(account, _)| account)
            .collect())
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<CollectionOutcome, ProviderError> {
        for attempt in 0..2 {
            let session = self
                .session_for_account(&account.external_account_id)
                .await?;
            match self
                .client
                .fetch(
                    &session.credential.cookie_header,
                    Some(
                        session
                            .credential
                            .account_hint
                            .as_deref()
                            .unwrap_or(&account.external_account_id),
                    ),
                )
                .await
            {
                Ok(fetch) => {
                    if !self
                        .cache
                        .generation_is_current(&account.external_account_id, session.generation)
                    {
                        continue;
                    }
                    if fetch
                        .identity
                        .as_ref()
                        .and_then(CursorUserInfo::stable_id)
                        .is_some_and(|id| id != account.external_account_id)
                    {
                        self.cache
                            .remove_if_generation(&account.external_account_id, session.generation);
                        return Err(ProviderError::new(
                            ProviderErrorKind::CredentialsInvalid,
                            "Cursor session identity changed while collecting usage",
                        ));
                    }
                    let normalized = normalize_cursor_fetch(
                        fetch,
                        session.credential.source,
                        &account.external_account_id,
                    )?;
                    return Ok(CollectionOutcome::collected_scoped_with_supplemental(
                        normalized.collection,
                        normalized.scope,
                        normalized.supplemental,
                    ));
                }
                Err(error) if error.kind() == ProviderErrorKind::Unauthorized && attempt == 0 => {
                    self.cache
                        .remove_if_generation(&account.external_account_id, session.generation);
                }
                Err(error) => return Err(error),
            }
        }
        Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Cursor rejected the refreshed session",
        ))
    }

    async fn invalidate_cached_credentials(
        &self,
        _profile_id: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.cache.clear();
        Ok(())
    }
}

fn discovered_account(external_account_id: &str, identity: &CursorUserInfo) -> DiscoveredAccount {
    DiscoveredAccount {
        external_account_id: external_account_id.to_string(),
        display_name: identity
            .display_name()
            .or_else(|| identity.email())
            .map(str::to_string),
        email: identity.email().map(str::to_string),
        profile_id: None,
    }
}

#[derive(Clone)]
struct CachedSession {
    credential: SessionCredential,
    generation: u64,
}

#[derive(Default)]
struct SessionCache {
    state: Mutex<SessionCacheState>,
}

#[derive(Default)]
struct SessionCacheState {
    next_generation: u64,
    accounts: BTreeMap<String, CachedSession>,
}

impl SessionCache {
    fn get(&self, account_id: &str) -> Option<CachedSession> {
        self.lock().accounts.get(account_id).cloned()
    }

    fn store(&self, account_id: String, credential: SessionCredential) -> CachedSession {
        let mut state = self.lock();
        if let Some(existing) = state.accounts.get(&account_id) {
            if existing.credential.cookie_header == credential.cookie_header {
                return existing.clone();
            }
        }
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let cached = CachedSession {
            credential,
            generation: state.next_generation,
        };
        state.accounts.insert(account_id, cached.clone());
        cached
    }

    fn generation_is_current(&self, account_id: &str, generation: u64) -> bool {
        self.lock()
            .accounts
            .get(account_id)
            .is_some_and(|session| session.generation == generation)
    }

    fn remove_if_generation(&self, account_id: &str, generation: u64) {
        let mut state = self.lock();
        if state
            .accounts
            .get(account_id)
            .is_some_and(|session| session.generation == generation)
        {
            state.accounts.remove(account_id);
        }
    }

    fn clear(&self) {
        self.lock().accounts.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionCacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests;
