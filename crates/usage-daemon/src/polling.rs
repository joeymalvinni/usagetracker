use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    panic::AssertUnwindSafe,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use futures_util::{future::join_all, FutureExt};
use tokio::{
    sync::{Mutex, Notify, RwLock},
    time::timeout,
};
use tracing::{debug, info, warn};
use usage_core::{
    AccountId, ProviderId, ProviderRefreshResult, ProviderRefreshStatus, RefreshJob, RefreshJobId,
    RefreshJobStatus, RefreshScope, RefreshTrigger,
};

use crate::{
    health,
    notifications::NotificationManager,
    providers::{
        AccountDiscoveryFailure, DiscoveredAccount, ProviderCollectionResult, ProviderCollector,
        ProviderError, ProviderErrorKind,
    },
    storage::{Storage, StoredProviderBackoff},
};

const RATE_LIMIT_BACKOFF_SECONDS: [i64; 5] = [5 * 60, 10 * 60, 20 * 60, 40 * 60, 60 * 60];
const PROVIDER_DISCOVERY_BUDGET: Duration = Duration::from_secs(30);
const DEFAULT_ACCOUNT_COLLECTION_BUDGET: Duration = Duration::from_secs(60);
const CLAUDE_ACCOUNT_COLLECTION_BUDGET: Duration = Duration::from_secs(75);
const RETAINED_REFRESH_JOBS: usize = 64;

type RateLimitBackoff = StoredProviderBackoff;

#[derive(Clone)]
pub struct RefreshCoordinator {
    storage: Storage,
    notifications: Arc<NotificationManager>,
    providers: Arc<RwLock<Vec<Arc<dyn ProviderCollector>>>>,
    jobs: Arc<Mutex<RefreshJobs>>,
    provider_flights: Arc<Mutex<HashMap<ProviderId, Arc<ProviderRefreshFlight>>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RefreshKey(Option<Vec<ProviderId>>);

impl RefreshKey {
    fn covers(&self, requested: &Self) -> bool {
        match (&self.0, &requested.0) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(active), Some(requested)) => {
                requested.iter().all(|provider| active.contains(provider))
            }
        }
    }
}

struct RefreshJobEntry {
    job: RwLock<RefreshJob>,
    finished: Notify,
}

struct ProviderRefreshFlight {
    result: RwLock<Option<Vec<ProviderRefreshResult>>>,
    finished: Notify,
}

#[derive(Default)]
struct RefreshJobs {
    active: HashMap<RefreshKey, Arc<RefreshJobEntry>>,
    by_id: HashMap<RefreshJobId, Arc<RefreshJobEntry>>,
    completed: VecDeque<RefreshJobId>,
}

pub struct StartedRefresh {
    pub job: RefreshJob,
    pub coalesced: bool,
}

impl RefreshCoordinator {
    #[cfg(test)]
    pub fn new(storage: Storage, providers: Vec<Arc<dyn ProviderCollector>>) -> Self {
        let notifications = NotificationManager::new(storage.clone(), false);
        Self::with_notifications(storage, providers, notifications)
    }

    pub fn with_notifications(
        storage: Storage,
        providers: Vec<Arc<dyn ProviderCollector>>,
        notifications: Arc<NotificationManager>,
    ) -> Self {
        Self {
            storage,
            notifications,
            providers: Arc::new(RwLock::new(providers)),
            jobs: Arc::new(Mutex::new(RefreshJobs::default())),
            provider_flights: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn set_providers(&self, providers: Vec<Arc<dyn ProviderCollector>>) {
        *self.providers.write().await = providers;
    }

    pub fn notification_manager(&self) -> Arc<NotificationManager> {
        self.notifications.clone()
    }

    /// Starts a manual refresh without tying the job lifetime to the socket
    /// request. An equivalent in-flight scope is returned instead of queued.
    pub async fn start_refresh(
        &self,
        filter: Option<Vec<ProviderId>>,
        trigger: RefreshTrigger,
    ) -> StartedRefresh {
        let (key, entry, coalesced) = self.claim_job(filter, trigger).await;
        let job = entry.job.read().await.clone();
        if !coalesced {
            self.spawn_claimed_job(key, entry);
        }
        StartedRefresh { job, coalesced }
    }

    pub async fn get_refresh_job(&self, job_id: &RefreshJobId) -> Option<RefreshJob> {
        let entry = self.jobs.lock().await.by_id.get(job_id).cloned()?;
        let job = entry.job.read().await.clone();
        Some(job)
    }

    /// Compatibility path for periodic and file-watcher callers. It claims the
    /// same job registry as the API and awaits shared work when one is active.
    pub async fn refresh(&self, filter: Option<&[ProviderId]>) -> RefreshReport {
        let (key, entry, coalesced) = self
            .claim_job(filter.map(<[ProviderId]>::to_vec), RefreshTrigger::System)
            .await;
        if !coalesced {
            self.spawn_claimed_job(key, entry.clone());
        }
        let job = wait_for_job(entry).await;
        RefreshReport::from_job(job)
    }

    async fn claim_job(
        &self,
        filter: Option<Vec<ProviderId>>,
        trigger: RefreshTrigger,
    ) -> (RefreshKey, Arc<RefreshJobEntry>, bool) {
        let scope = normalized_scope(filter);
        let key = RefreshKey(scope.providers.clone());
        let mut jobs = self.jobs.lock().await;
        if let Some((active_key, entry)) = jobs
            .active
            .iter()
            .find(|(active_key, _)| active_key.covers(&key))
        {
            return (active_key.clone(), entry.clone(), true);
        }

        let job = RefreshJob {
            id: RefreshJobId::new(uuid::Uuid::new_v4().to_string()),
            scope,
            trigger,
            status: RefreshJobStatus::Queued,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            provider_results: Vec::new(),
            failure_message: None,
        };
        let entry = Arc::new(RefreshJobEntry {
            job: RwLock::new(job.clone()),
            finished: Notify::new(),
        });
        jobs.active.insert(key.clone(), entry.clone());
        jobs.by_id.insert(job.id, entry.clone());
        (key, entry, false)
    }

    fn spawn_claimed_job(&self, key: RefreshKey, entry: Arc<RefreshJobEntry>) {
        let coordinator = self.clone();
        tokio::spawn(async move {
            let result = AssertUnwindSafe(coordinator.run_claimed_job(key.clone(), entry.clone()))
                .catch_unwind()
                .await;
            if result.is_err() {
                coordinator
                    .fail_claimed_job(key, entry, "refresh task panicked")
                    .await;
            }
        });
    }

    async fn run_claimed_job(&self, key: RefreshKey, entry: Arc<RefreshJobEntry>) {
        {
            let mut job = entry.job.write().await;
            job.status = RefreshJobStatus::Running;
            job.started_at = Some(Utc::now());
        }
        let filter = entry.job.read().await.scope.providers.clone();
        let report = self.execute_refresh(filter.as_deref()).await;
        let job_id = {
            let mut job = entry.job.write().await;
            job.status = RefreshJobStatus::Completed;
            job.started_at = Some(report.started_at);
            job.finished_at = Some(report.finished_at);
            job.provider_results = report.provider_results;
            job.id.clone()
        };
        entry.finished.notify_waiters();

        self.retain_finished_job(key, entry, job_id).await;
    }

    async fn fail_claimed_job(&self, key: RefreshKey, entry: Arc<RefreshJobEntry>, message: &str) {
        let job_id = {
            let mut job = entry.job.write().await;
            if job.status.is_terminal() {
                return;
            }
            job.status = RefreshJobStatus::Failed;
            job.finished_at = Some(Utc::now());
            job.failure_message = Some(message.to_string());
            job.id.clone()
        };
        entry.finished.notify_waiters();
        self.retain_finished_job(key, entry, job_id).await;
    }

    async fn retain_finished_job(
        &self,
        key: RefreshKey,
        entry: Arc<RefreshJobEntry>,
        job_id: RefreshJobId,
    ) {
        let mut jobs = self.jobs.lock().await;
        if jobs
            .active
            .get(&key)
            .is_some_and(|active| Arc::ptr_eq(active, &entry))
        {
            jobs.active.remove(&key);
        }
        jobs.completed.push_back(job_id);
        while jobs.completed.len() > RETAINED_REFRESH_JOBS {
            if let Some(expired) = jobs.completed.pop_front() {
                jobs.by_id.remove(&expired);
            }
        }
    }

    async fn execute_refresh(&self, filter: Option<&[ProviderId]>) -> RefreshReport {
        let providers = self.providers.read().await.clone();
        let started_at = Utc::now();
        let filter_values = filter
            .map(|ids| ids.iter().map(ProviderId::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        info!(
            provider_filter = ?filter_values,
            provider_count = providers.len(),
            "refresh started"
        );

        let refreshes = providers.into_iter().filter_map(|provider| {
            let provider_id = provider.provider_id();
            if should_refresh_provider(&provider_id, filter) {
                Some(self.refresh_provider_once(provider, provider_id))
            } else {
                debug!(
                    provider_id = provider_id.as_str(),
                    "skipping provider outside refresh filter"
                );
                None
            }
        });
        let provider_results = join_all(refreshes)
            .await
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        let finished_at = Utc::now();
        info!(
            results = provider_results.len(),
            elapsed_ms = (finished_at - started_at).num_milliseconds(),
            "refresh finished"
        );
        RefreshReport {
            started_at,
            finished_at,
            provider_results,
        }
    }

    /// Shares provider work across every overlapping refresh job. Scope-level
    /// coalescing can return the same job for covered requests, while this
    /// provider-level registry handles partial overlaps (for example `codex`
    /// followed by `all`) without issuing the provider call twice.
    async fn refresh_provider_once(
        &self,
        provider: Arc<dyn ProviderCollector>,
        provider_id: ProviderId,
    ) -> Vec<ProviderRefreshResult> {
        let (flight, claimed) = {
            let mut flights = self.provider_flights.lock().await;
            if let Some(flight) = flights.get(&provider_id) {
                (flight.clone(), false)
            } else {
                let flight = Arc::new(ProviderRefreshFlight {
                    result: RwLock::new(None),
                    finished: Notify::new(),
                });
                flights.insert(provider_id.clone(), flight.clone());
                (flight, true)
            }
        };

        if claimed {
            let coordinator = self.clone();
            let task_flight = flight.clone();
            tokio::spawn(async move {
                let result =
                    AssertUnwindSafe(coordinator.refresh_provider(provider, provider_id.clone()))
                        .catch_unwind()
                        .await;
                let result = match result {
                    Ok(result) => result,
                    Err(_) => {
                        vec![
                            coordinator
                                .record_failure(
                                    provider_id.clone(),
                                    None,
                                    ProviderError::new(
                                        ProviderErrorKind::ProviderUnavailable,
                                        "provider refresh panicked",
                                    ),
                                )
                                .await,
                        ]
                    }
                };
                *task_flight.result.write().await = Some(result);
                task_flight.finished.notify_waiters();

                let mut flights = coordinator.provider_flights.lock().await;
                if flights
                    .get(&provider_id)
                    .is_some_and(|active| Arc::ptr_eq(active, &task_flight))
                {
                    flights.remove(&provider_id);
                }
            });
        }

        wait_for_provider_refresh(flight).await
    }

    async fn refresh_provider(
        &self,
        provider: Arc<dyn ProviderCollector>,
        provider_id: ProviderId,
    ) -> Vec<ProviderRefreshResult> {
        debug!(
            provider_id = provider_id.as_str(),
            "discovering provider accounts"
        );
        let discovery_started = Instant::now();

        let discovery = match timeout(PROVIDER_DISCOVERY_BUDGET, provider.discover_accounts()).await
        {
            Ok(Ok(discovery)) => discovery,
            Ok(Err(err)) => return vec![self.record_failure(provider_id, None, err).await],
            Err(_) => {
                let message = "Finding accounts took too long. Try again.".to_string();
                return vec![
                    self.record_failure(
                        provider_id,
                        None,
                        ProviderError::new(ProviderErrorKind::ProviderUnavailable, message),
                    )
                    .await,
                ];
            }
        };

        info!(
            provider_id = provider_id.as_str(),
            account_count = discovery.accounts.len(),
            failure_count = discovery.failures.len(),
            elapsed_ms = discovery_started.elapsed().as_millis(),
            "provider account discovery completed"
        );

        let mut results = join_all(discovery.accounts.into_iter().map(|discovered| {
            self.refresh_account(provider.as_ref(), provider_id.clone(), discovered)
        }))
        .await;
        results.extend(
            self.record_account_discovery_failures(&provider_id, discovery.failures)
                .await,
        );
        results
    }

    async fn record_account_discovery_failures(
        &self,
        provider_id: &ProviderId,
        failures: Vec<AccountDiscoveryFailure>,
    ) -> Vec<ProviderRefreshResult> {
        if failures.is_empty() {
            return Vec::new();
        }
        let accounts = match self.storage.accounts().await {
            Ok(accounts) => accounts,
            Err(err) => {
                warn!(
                    provider_id = provider_id.as_str(),
                    error = %err,
                    "failed to load accounts for profile discovery failures"
                );
                return vec![storage_error_result(
                    provider_id.clone(),
                    None,
                    format!("failed to load accounts for discovery failures: {err}"),
                )];
            }
        };
        let mut results = Vec::new();
        for failure in failures {
            let Some(account) = accounts.iter().find(|account| {
                account.provider_id == *provider_id
                    && account.profile_id.as_deref() == Some(failure.profile_id.as_str())
            }) else {
                debug!(
                    provider_id = provider_id.as_str(),
                    profile_id = failure.profile_id.as_str(),
                    "ignoring discovery failure for a pending profile"
                );
                continue;
            };
            if !account.collection_enabled {
                continue;
            }
            results.push(
                self.record_failure(provider_id.clone(), Some(account.id.clone()), failure.error)
                    .await,
            );
        }
        results
    }

    async fn refresh_account(
        &self,
        provider: &dyn ProviderCollector,
        provider_id: ProviderId,
        discovered: DiscoveredAccount,
    ) -> ProviderRefreshResult {
        let collection_budget = account_collection_budget(&provider_id);
        self.refresh_account_with_budget(provider, provider_id, discovered, collection_budget)
            .await
    }

    async fn refresh_account_with_budget(
        &self,
        provider: &dyn ProviderCollector,
        provider_id: ProviderId,
        discovered: DiscoveredAccount,
        collection_budget: Duration,
    ) -> ProviderRefreshResult {
        let account = match self
            .storage
            .upsert_account(
                &provider_id,
                &discovered.external_account_id,
                discovered.profile_id.as_deref(),
                discovered.display_name.as_deref(),
                discovered.email.as_deref(),
            )
            .await
        {
            Ok(account) => account,
            Err(err) => {
                warn!(
                    provider_id = provider_id.as_str(),
                    error = %err,
                    "failed to store provider account"
                );
                return storage_error_result(
                    provider_id,
                    None,
                    format!("failed to store account: {err}"),
                );
            }
        };

        if !account.collection_enabled {
            self.clear_rate_limit_backoff(&provider_id, &account.id)
                .await;
            debug!(
                provider_id = provider_id.as_str(),
                account_id = account.id.as_str(),
                "skipping disabled provider account"
            );
            let disabled = health::disabled(provider_id.clone());
            let disabled = usage_core::ProviderHealth {
                account_id: Some(account.id.clone()),
                ..disabled
            };
            if let Err(err) = self.storage.upsert_health(&disabled).await {
                warn!(
                    provider_id = provider_id.as_str(),
                    account_id = account.id.as_str(),
                    error = %err,
                    "failed to store disabled account health"
                );
            }
            return ProviderRefreshResult {
                provider_id,
                account_id: Some(account.id),
                status: ProviderRefreshStatus::Disabled,
                collection_mode: None,
                collected_at: None,
                message: Some("account collection disabled".to_string()),
            };
        }

        if let Some(backoff) = self
            .active_rate_limit_backoff(&provider_id, &account.id, Utc::now())
            .await
        {
            return self
                .record_backing_off(provider_id, account.id, backoff)
                .await;
        }

        debug!(
            provider_id = provider_id.as_str(),
            account_id = account.id.as_str(),
            "collecting provider usage"
        );
        let collect_started = Instant::now();

        match timeout(collection_budget, provider.collect_usage(&discovered)).await {
            Ok(Ok(result)) => {
                self.clear_rate_limit_backoff(&provider_id, &account.id)
                    .await;
                info!(
                    provider_id = provider_id.as_str(),
                    account_id = account.id.as_str(),
                    collection_mode = result.collection_mode.as_str(),
                    elapsed_ms = collect_started.elapsed().as_millis(),
                    warnings = result.warnings.len(),
                    "provider usage collection completed"
                );
                self.store_success(provider_id, account, result).await
            }
            Ok(Err(err)) => {
                let backoff = if err.kind() == ProviderErrorKind::RateLimited {
                    let provider_retry_at = err.retry_at();
                    Some(
                        self.note_rate_limit(
                            &provider_id,
                            &account.id,
                            Utc::now(),
                            err.short_message(),
                            provider_retry_at,
                        )
                        .await,
                    )
                } else {
                    None
                };
                warn!(
                    provider_id = provider_id.as_str(),
                    account_id = account.id.as_str(),
                    elapsed_ms = collect_started.elapsed().as_millis(),
                    error_code = err.kind().as_str(),
                    error = %err,
                    "provider usage collection failed"
                );
                if let Some(backoff) = backoff {
                    warn!(
                        provider_id = provider_id.as_str(),
                        account_id = account.id.as_str(),
                        consecutive_rate_limits = backoff.consecutive_failures,
                        retry_at = %backoff.retry_at,
                        "provider rate-limit backoff scheduled"
                    );
                }
                self.record_failure(provider_id, Some(account.id), err)
                    .await
            }
            Err(_) => {
                let err = ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    "Usage refresh took too long. Try again.",
                );
                warn!(
                    provider_id = provider_id.as_str(),
                    account_id = account.id.as_str(),
                    elapsed_ms = collect_started.elapsed().as_millis(),
                    "provider usage collection timed out"
                );
                self.record_failure(provider_id, Some(account.id), err)
                    .await
            }
        }
    }

    async fn active_rate_limit_backoff(
        &self,
        provider_id: &ProviderId,
        account_id: &AccountId,
        now: DateTime<Utc>,
    ) -> Option<RateLimitBackoff> {
        match self.storage.provider_backoff(provider_id, account_id).await {
            Ok(backoff) => backoff.filter(|backoff| now < backoff.retry_at),
            Err(err) => {
                warn!(error = %err, "failed to read persisted provider backoff");
                None
            }
        }
    }

    async fn note_rate_limit(
        &self,
        provider_id: &ProviderId,
        account_id: &AccountId,
        now: DateTime<Utc>,
        error_message: &str,
        provider_retry_at: Option<DateTime<Utc>>,
    ) -> RateLimitBackoff {
        let previous = match self.storage.provider_backoff(provider_id, account_id).await {
            Ok(previous) => previous,
            Err(err) => {
                warn!(error = %err, "failed to read provider backoff before update");
                None
            }
        };
        let consecutive_failures = previous
            .as_ref()
            .map(|backoff| backoff.consecutive_failures.saturating_add(1))
            .unwrap_or(1);
        let delay_index = consecutive_failures
            .saturating_sub(1)
            .min(RATE_LIMIT_BACKOFF_SECONDS.len() - 1);
        let default_retry_at = now
            + chrono::TimeDelta::seconds(jittered_backoff_seconds(
                RATE_LIMIT_BACKOFF_SECONDS[delay_index],
                provider_id,
                account_id,
                consecutive_failures,
            ));
        let backoff = RateLimitBackoff {
            provider_id: provider_id.clone(),
            account_id: account_id.clone(),
            consecutive_failures,
            retry_at: provider_retry_at
                .filter(|retry_at| *retry_at > default_retry_at)
                .unwrap_or(default_retry_at),
            last_failure_at: now,
            error_message: error_message.to_string(),
        };
        if let Err(err) = self.storage.upsert_provider_backoff(&backoff).await {
            warn!(error = %err, "failed to persist provider backoff");
        }
        backoff
    }

    async fn clear_rate_limit_backoff(&self, provider_id: &ProviderId, account_id: &AccountId) {
        if let Err(err) = self
            .storage
            .delete_provider_backoff(provider_id, account_id)
            .await
        {
            warn!(error = %err, "failed to clear persisted provider backoff");
        }
    }

    async fn record_backing_off(
        &self,
        provider_id: ProviderId,
        account_id: AccountId,
        backoff: RateLimitBackoff,
    ) -> ProviderRefreshResult {
        let message = format!(
            "{}; retrying after {}",
            backoff.error_message,
            backoff.retry_at.to_rfc3339()
        );
        info!(
            provider_id = provider_id.as_str(),
            account_id = account_id.as_str(),
            consecutive_rate_limits = backoff.consecutive_failures,
            retry_at = %backoff.retry_at,
            "skipping provider collection during rate-limit backoff"
        );
        let provider_health = health::backing_off(
            provider_id.clone(),
            account_id.clone(),
            backoff.last_failure_at,
            message.clone(),
        );
        if let Err(err) = self.storage.upsert_health(&provider_health).await {
            warn!(error = %err, "failed to store provider backoff health");
        }
        ProviderRefreshResult {
            provider_id,
            account_id: Some(account_id),
            status: ProviderRefreshStatus::RateLimited,
            collection_mode: None,
            collected_at: None,
            message: Some(message),
        }
    }

    async fn store_success(
        &self,
        provider_id: ProviderId,
        account: usage_core::Account,
        result: ProviderCollectionResult,
    ) -> ProviderRefreshResult {
        let daily_usage_days = result.daily_usage.len();
        let snapshot = result.usage.into_snapshot(account.id.clone());
        if snapshot.provider_id != provider_id {
            warn!(
                provider_id = provider_id.as_str(),
                snapshot_provider_id = snapshot.provider_id.as_str(),
                account_id = account.id.as_str(),
                "provider returned usage for a different provider id"
            );
            return provider_error_result(
                provider_id,
                Some(account.id),
                ProviderError::new(
                    ProviderErrorKind::Parse,
                    "provider usage payload had a mismatched provider id",
                ),
            );
        }

        let store_started = Instant::now();
        let ok_health = health::ok(
            provider_id.clone(),
            account.id.clone(),
            result.collection_mode.clone(),
        );
        if let Err(err) = self
            .storage
            .record_success(
                &snapshot,
                &result.daily_usage,
                &ok_health,
                result.account_email.as_deref(),
            )
            .await
        {
            warn!(
                provider_id = provider_id.as_str(),
                account_id = account.id.as_str(),
                error = %err,
                "failed to atomically store provider refresh"
            );
            return storage_error_result(
                provider_id,
                Some(account.id),
                format!("failed to store provider refresh: {err}"),
            );
        }

        let forecasts = match self
            .storage
            .forecast_history(&snapshot, Utc::now() - chrono::TimeDelta::days(30), 96)
            .await
        {
            Ok(history) => crate::forecast::forecast_snapshot(&snapshot, &history, Utc::now()),
            Err(err) => {
                warn!(error = %err, "failed to load notification forecast history");
                Vec::new()
            }
        };
        self.notifications
            .process_snapshot_with_forecasts(&account, &snapshot, &forecasts)
            .await;

        for warning in &result.warnings {
            warn!(
                provider_id = provider_id.as_str(),
                account_id = account.id.as_str(),
                warning = %warning,
                "provider refresh warning"
            );
        }

        info!(
            provider_id = provider_id.as_str(),
            account_id = account.id.as_str(),
            windows = snapshot.windows.len(),
            daily_usage_days,
            collection_mode = result.collection_mode.as_str(),
            elapsed_ms = store_started.elapsed().as_millis(),
            "provider usage stored"
        );
        ProviderRefreshResult {
            provider_id,
            account_id: Some(account.id),
            status: ProviderRefreshStatus::Ok,
            collection_mode: Some(result.collection_mode),
            collected_at: Some(snapshot.collected_at),
            message: result.warnings.first().cloned(),
        }
    }

    async fn record_failure(
        &self,
        provider_id: ProviderId,
        account_id: Option<AccountId>,
        error: ProviderError,
    ) -> ProviderRefreshResult {
        warn!(
            provider_id = provider_id.as_str(),
            account_id = account_id.as_ref().map(AccountId::as_str),
            error_code = error.kind().as_str(),
            error = %error,
            "provider refresh failed"
        );
        let provider_health =
            health::from_provider_error(provider_id.clone(), account_id.clone(), &error);
        if let Err(err) = self.storage.upsert_health(&provider_health).await {
            warn!(error = %err, "failed to store provider error health");
        }
        provider_error_result(provider_id, account_id, error)
    }
}

fn jittered_backoff_seconds(
    base_seconds: i64,
    provider_id: &ProviderId,
    account_id: &AccountId,
    consecutive_failures: usize,
) -> i64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    provider_id.hash(&mut hasher);
    account_id.hash(&mut hasher);
    consecutive_failures.hash(&mut hasher);
    // A stable 80–120% spread prevents a fleet of clients from retrying in a
    // synchronized wave while keeping retry behavior deterministic in tests.
    let basis_points = 8_000 + i64::try_from(hasher.finish() % 4_001).unwrap_or(0);
    (base_seconds.saturating_mul(basis_points) / 10_000).min(60 * 60)
}

pub struct RefreshReport {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub provider_results: Vec<ProviderRefreshResult>,
}

impl RefreshReport {
    fn from_job(job: RefreshJob) -> Self {
        Self {
            started_at: job.started_at.unwrap_or(job.created_at),
            finished_at: job.finished_at.unwrap_or_else(Utc::now),
            provider_results: job.provider_results,
        }
    }
}

fn normalized_scope(filter: Option<Vec<ProviderId>>) -> RefreshScope {
    match filter {
        Some(providers) => RefreshScope::providers(providers),
        None => RefreshScope::all(),
    }
}

async fn wait_for_job(entry: Arc<RefreshJobEntry>) -> RefreshJob {
    loop {
        let notified = entry.finished.notified();
        let job = entry.job.read().await.clone();
        if job.status.is_terminal() {
            return job;
        }
        notified.await;
    }
}

async fn wait_for_provider_refresh(
    flight: Arc<ProviderRefreshFlight>,
) -> Vec<ProviderRefreshResult> {
    loop {
        let notified = flight.finished.notified();
        if let Some(result) = flight.result.read().await.clone() {
            return result;
        }
        notified.await;
    }
}

fn account_collection_budget(provider_id: &ProviderId) -> Duration {
    match provider_id.as_str() {
        "claude" => CLAUDE_ACCOUNT_COLLECTION_BUDGET,
        _ => DEFAULT_ACCOUNT_COLLECTION_BUDGET,
    }
}

fn should_refresh_provider(provider_id: &ProviderId, filter: Option<&[ProviderId]>) -> bool {
    filter.is_none_or(|filter| filter.iter().any(|id| id == provider_id))
}

fn provider_error_result(
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    error: ProviderError,
) -> ProviderRefreshResult {
    ProviderRefreshResult {
        provider_id,
        account_id,
        status: error.kind().into(),
        collection_mode: None,
        collected_at: None,
        message: Some(error.short_message().to_string()),
    }
}

fn storage_error_result(
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    message: String,
) -> ProviderRefreshResult {
    ProviderRefreshResult {
        provider_id,
        account_id,
        status: ProviderRefreshStatus::StorageError,
        collection_mode: None,
        collected_at: None,
        message: Some(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use serde_json::json;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::{
        sync::{Barrier, Notify},
        time::timeout,
    };
    use usage_core::{
        ProviderHealth, ProviderHealthStatus, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind,
    };
    use uuid::Uuid;

    use crate::providers::{
        AccountDiscovery, AccountDiscoveryFailure, ProviderCollectionResult, ProviderUsage,
    };

    struct FakeProvider;

    #[async_trait]
    impl ProviderCollector for FakeProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("claude")
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            Ok(vec![DiscoveredAccount {
                external_account_id: "external-account".to_string(),
                display_name: None,
                email: None,
                profile_id: None,
            }]
            .into())
        }

        async fn collect_usage(
            &self,
            _account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            Ok(ProviderCollectionResult {
                usage: ProviderUsage {
                    provider_id: ProviderId::new("claude"),
                    collected_at: Utc::now(),
                    windows: vec![UsageWindow {
                        window_id: "claude_usage".to_string(),
                        label: "Claude usage".to_string(),
                        kind: UsageWindowKind::Daily,
                        used: Some(UsageAmount {
                            value: 25.0,
                            unit: UsageUnit::Percent,
                        }),
                        limit: Some(UsageAmount {
                            value: 100.0,
                            unit: UsageUnit::Percent,
                        }),
                        remaining: Some(UsageAmount {
                            value: 75.0,
                            unit: UsageUnit::Percent,
                        }),
                        percent_used: Some(25.0),
                        percent_remaining: Some(75.0),
                        reset_at: None,
                    }],
                    metadata: json!({}),
                },
                daily_usage: Vec::new(),
                collection_mode: "live".to_string(),
                account_email: Some("claude@example.com".to_string()),
                warnings: vec![],
            })
        }
    }

    struct MultiAccountProvider;

    struct MixedDiscoveryProvider;

    #[async_trait]
    impl ProviderCollector for MixedDiscoveryProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("grok")
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            Ok(AccountDiscovery {
                accounts: vec![DiscoveredAccount {
                    external_account_id: "healthy-user".to_string(),
                    display_name: None,
                    email: Some("healthy@example.com".to_string()),
                    profile_id: Some("healthy".to_string()),
                }],
                failures: vec![AccountDiscoveryFailure {
                    profile_id: "broken".to_string(),
                    error: ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        "broken Grok credentials",
                    ),
                }],
            })
        }

        async fn collect_usage(
            &self,
            account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            let mut result = FakeProvider.collect_usage(account).await?;
            result.usage.provider_id = ProviderId::new("grok");
            Ok(result)
        }
    }

    #[async_trait]
    impl ProviderCollector for MultiAccountProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("codex")
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            Ok(vec![
                DiscoveredAccount {
                    external_account_id: "same-openai-account".to_string(),
                    display_name: Some("Personal".to_string()),
                    email: Some("personal@example.com".to_string()),
                    profile_id: Some("personal".to_string()),
                },
                DiscoveredAccount {
                    external_account_id: "same-openai-account".to_string(),
                    display_name: Some("Work".to_string()),
                    email: Some("work@example.com".to_string()),
                    profile_id: Some("work".to_string()),
                },
            ]
            .into())
        }

        async fn collect_usage(
            &self,
            account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            Ok(ProviderCollectionResult {
                usage: ProviderUsage {
                    provider_id: ProviderId::new("codex"),
                    collected_at: Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({
                        "credential_profile": account.profile_id.as_deref(),
                    }),
                },
                daily_usage: Vec::new(),
                collection_mode: "test".to_string(),
                account_email: account.email.clone(),
                warnings: vec![],
            })
        }
    }

    struct ConcurrentAccountProvider {
        barrier: Arc<Barrier>,
    }

    #[async_trait]
    impl ProviderCollector for ConcurrentAccountProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("codex")
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            Ok(["personal", "work"]
                .into_iter()
                .map(|profile| DiscoveredAccount {
                    external_account_id: profile.to_string(),
                    display_name: None,
                    email: None,
                    profile_id: Some(profile.to_string()),
                })
                .collect())
        }

        async fn collect_usage(
            &self,
            _account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            self.barrier.wait().await;
            Ok(ProviderCollectionResult {
                usage: ProviderUsage {
                    provider_id: ProviderId::new("codex"),
                    collected_at: Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({}),
                },
                daily_usage: Vec::new(),
                collection_mode: "test".to_string(),
                account_email: None,
                warnings: Vec::new(),
            })
        }
    }

    struct ConcurrentDiscoveryProvider {
        provider_id: &'static str,
        barrier: Arc<Barrier>,
    }

    struct RateLimitedProvider {
        attempts: Arc<AtomicUsize>,
    }

    struct BlockingProvider {
        provider_id: &'static str,
        attempts: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl ProviderCollector for BlockingProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new(self.provider_id)
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            Ok(vec![DiscoveredAccount {
                external_account_id: format!("{}-coalesced-account", self.provider_id),
                display_name: None,
                email: None,
                profile_id: Some("default".to_string()),
            }]
            .into())
        }

        async fn collect_usage(
            &self,
            _account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();
            self.release.notified().await;
            Ok(ProviderCollectionResult {
                usage: ProviderUsage {
                    provider_id: ProviderId::new(self.provider_id),
                    collected_at: Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({}),
                },
                daily_usage: Vec::new(),
                collection_mode: "test".to_string(),
                account_email: None,
                warnings: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl ProviderCollector for RateLimitedProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("claude")
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            Ok(vec![DiscoveredAccount {
                external_account_id: "rate-limited-account".to_string(),
                display_name: None,
                email: None,
                profile_id: Some("default".to_string()),
            }]
            .into())
        }

        async fn collect_usage(
            &self,
            _account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "Claude usage endpoint is rate limited",
            ))
        }
    }

    #[async_trait]
    impl ProviderCollector for ConcurrentDiscoveryProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new(self.provider_id)
        }

        async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
            self.barrier.wait().await;
            Ok(vec![DiscoveredAccount {
                external_account_id: self.provider_id.to_string(),
                display_name: None,
                email: None,
                profile_id: Some("default".to_string()),
            }]
            .into())
        }

        async fn collect_usage(
            &self,
            _account: &DiscoveredAccount,
        ) -> Result<ProviderCollectionResult, ProviderError> {
            Ok(ProviderCollectionResult {
                usage: ProviderUsage {
                    provider_id: ProviderId::new(self.provider_id),
                    collected_at: Utc::now(),
                    windows: Vec::new(),
                    metadata: json!({}),
                },
                daily_usage: Vec::new(),
                collection_mode: "test".to_string(),
                account_email: None,
                warnings: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn background_refresh_returns_immediately_and_coalesces_covered_scopes() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider = Arc::new(BlockingProvider {
            provider_id: "codex",
            attempts: attempts.clone(),
            started: started.clone(),
            release: release.clone(),
        });
        let coordinator = Arc::new(RefreshCoordinator::new(test_storage(), vec![provider]));

        let first_started = started.notified();
        let first = coordinator
            .start_refresh(None, RefreshTrigger::Manual)
            .await;
        assert!(!first.coalesced);
        assert_eq!(first.job.status, RefreshJobStatus::Queued);
        timeout(Duration::from_secs(1), first_started)
            .await
            .expect("background refresh should begin");

        let second = coordinator
            .start_refresh(Some(vec![ProviderId::new("codex")]), RefreshTrigger::Manual)
            .await;
        assert!(second.coalesced);
        assert_eq!(second.job.id, first.job.id);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        let shared_waiter = {
            let coordinator = coordinator.clone();
            tokio::spawn(
                async move { coordinator.refresh(Some(&[ProviderId::new("codex")])).await },
            )
        };
        tokio::task::yield_now().await;
        release.notify_waiters();
        let report = timeout(Duration::from_secs(1), shared_waiter)
            .await
            .expect("shared refresh should finish")
            .unwrap();
        assert_eq!(report.provider_results.len(), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        let finished = coordinator
            .get_refresh_job(&first.job.id)
            .await
            .expect("completed job should remain queryable");
        assert_eq!(finished.status, RefreshJobStatus::Completed);
    }

    #[tokio::test]
    async fn subset_then_superset_runs_only_uncovered_providers() {
        let codex_attempts = Arc::new(AtomicUsize::new(0));
        let codex_started = Arc::new(Notify::new());
        let codex_release = Arc::new(Notify::new());
        let coordinator = Arc::new(RefreshCoordinator::new(
            test_storage(),
            vec![
                Arc::new(BlockingProvider {
                    provider_id: "codex",
                    attempts: codex_attempts.clone(),
                    started: codex_started.clone(),
                    release: codex_release.clone(),
                }),
                Arc::new(FakeProvider),
            ],
        ));

        let started = codex_started.notified();
        coordinator
            .start_refresh(Some(vec![ProviderId::new("codex")]), RefreshTrigger::Manual)
            .await;
        timeout(Duration::from_secs(1), started).await.unwrap();

        let superset = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move {
                coordinator
                    .refresh(Some(&[ProviderId::new("codex"), ProviderId::new("claude")]))
                    .await
            })
        };
        tokio::task::yield_now().await;
        codex_release.notify_waiters();

        let report = timeout(Duration::from_secs(1), superset)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.provider_results.len(), 2);
        assert_eq!(codex_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_then_all_does_not_repeat_the_active_provider() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let coordinator = Arc::new(RefreshCoordinator::new(
            test_storage(),
            vec![
                Arc::new(BlockingProvider {
                    provider_id: "codex",
                    attempts: attempts.clone(),
                    started: started.clone(),
                    release: release.clone(),
                }),
                Arc::new(FakeProvider),
            ],
        ));

        let provider_started = started.notified();
        coordinator
            .start_refresh(Some(vec![ProviderId::new("codex")]), RefreshTrigger::Manual)
            .await;
        timeout(Duration::from_secs(1), provider_started)
            .await
            .unwrap();
        let all = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.refresh(None).await })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        let report = timeout(Duration::from_secs(1), all).await.unwrap().unwrap();
        assert_eq!(report.provider_results.len(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multiple_active_providers_then_all_joins_every_active_provider() {
        let codex_attempts = Arc::new(AtomicUsize::new(0));
        let claude_attempts = Arc::new(AtomicUsize::new(0));
        let codex_started = Arc::new(Notify::new());
        let claude_started = Arc::new(Notify::new());
        let codex_release = Arc::new(Notify::new());
        let claude_release = Arc::new(Notify::new());
        let coordinator = Arc::new(RefreshCoordinator::new(
            test_storage(),
            vec![
                Arc::new(BlockingProvider {
                    provider_id: "codex",
                    attempts: codex_attempts.clone(),
                    started: codex_started.clone(),
                    release: codex_release.clone(),
                }),
                Arc::new(BlockingProvider {
                    provider_id: "claude",
                    attempts: claude_attempts.clone(),
                    started: claude_started.clone(),
                    release: claude_release.clone(),
                }),
            ],
        ));

        let codex_wait = codex_started.notified();
        let claude_wait = claude_started.notified();
        coordinator
            .start_refresh(Some(vec![ProviderId::new("codex")]), RefreshTrigger::Manual)
            .await;
        coordinator
            .start_refresh(
                Some(vec![ProviderId::new("claude")]),
                RefreshTrigger::Manual,
            )
            .await;
        timeout(Duration::from_secs(1), async {
            tokio::join!(codex_wait, claude_wait);
        })
        .await
        .unwrap();

        let all = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.refresh(None).await })
        };
        tokio::task::yield_now().await;
        codex_release.notify_waiters();
        claude_release.notify_waiters();

        let report = timeout(Duration::from_secs(1), all).await.unwrap().unwrap();
        assert_eq!(report.provider_results.len(), 2);
        assert_eq!(codex_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(claude_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_refresh_clears_stale_provider_level_health() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        storage
            .upsert_health(&ProviderHealth {
                provider_id: provider_id.clone(),
                account_id: None,
                status: ProviderHealthStatus::CredentialsMissing,
                collection_mode: None,
                last_success_at: None,
                last_failure_at: Some(Utc::now()),
                last_error_code: Some("credentials_missing".to_string()),
                last_error_message: Some("missing".to_string()),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let coordinator = RefreshCoordinator::new(storage.clone(), vec![Arc::new(FakeProvider)]);
        let report = coordinator.refresh(None).await;

        assert_eq!(report.provider_results.len(), 1);
        assert_eq!(report.provider_results[0].status, ProviderRefreshStatus::Ok);
        let health = storage.provider_health().await.unwrap();
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].provider_id, provider_id);
        assert!(health[0].account_id.is_some());
        assert!(matches!(health[0].status, ProviderHealthStatus::Ok));
    }

    #[tokio::test]
    async fn mixed_discovery_updates_health_for_the_failed_existing_profile() {
        let storage = test_storage();
        let provider_id = ProviderId::new("grok");
        let healthy = storage
            .upsert_account(&provider_id, "healthy-user", Some("healthy"), None, None)
            .await
            .unwrap();
        let broken = storage
            .upsert_account(&provider_id, "broken-user", Some("broken"), None, None)
            .await
            .unwrap();
        let coordinator =
            RefreshCoordinator::new(storage.clone(), vec![Arc::new(MixedDiscoveryProvider)]);

        let report = coordinator.refresh(None).await;

        assert_eq!(report.provider_results.len(), 2);
        assert!(report.provider_results.iter().any(|result| {
            result.account_id.as_ref() == Some(&healthy.id)
                && result.status == ProviderRefreshStatus::Ok
        }));
        assert!(report.provider_results.iter().any(|result| {
            result.account_id.as_ref() == Some(&broken.id)
                && result.status == ProviderRefreshStatus::CredentialsInvalid
        }));
        let health = storage.provider_health().await.unwrap();
        assert!(health.iter().any(|entry| {
            entry.account_id.as_ref() == Some(&healthy.id)
                && matches!(entry.status, ProviderHealthStatus::Ok)
        }));
        assert!(health.iter().any(|entry| {
            entry.account_id.as_ref() == Some(&broken.id)
                && matches!(entry.status, ProviderHealthStatus::AuthFailed)
        }));
    }

    #[tokio::test]
    async fn refresh_rejects_duplicate_external_accounts_from_distinct_profiles() {
        let storage = test_storage();
        let coordinator =
            RefreshCoordinator::new(storage.clone(), vec![Arc::new(MultiAccountProvider)]);

        let report = coordinator.refresh(None).await;

        assert_eq!(report.provider_results.len(), 2);
        assert_eq!(
            report
                .provider_results
                .iter()
                .filter(|result| result.status == ProviderRefreshStatus::Ok)
                .count(),
            1
        );
        let conflict = report
            .provider_results
            .iter()
            .find(|result| result.status == ProviderRefreshStatus::StorageError)
            .expect("duplicate identity should be reported as a storage error");
        assert!(conflict
            .message
            .as_deref()
            .is_some_and(|message| message.contains("is already connected through profile")));

        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].external_account_id, "same-openai-account");

        let snapshots = storage.latest_usage().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            snapshots
                .iter()
                .filter(|snapshot| snapshot.provider_id.as_str() == "codex")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn refresh_skips_disabled_accounts() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        let account = storage
            .upsert_account(&provider_id, "external-account", None, Some("Claude"), None)
            .await
            .unwrap();
        storage
            .update_account(&account.id, None, None, Some(false))
            .await
            .unwrap();

        let coordinator = RefreshCoordinator::new(storage.clone(), vec![Arc::new(FakeProvider)]);
        let report = coordinator.refresh(None).await;

        assert_eq!(report.provider_results.len(), 1);
        assert_eq!(
            report.provider_results[0].status,
            ProviderRefreshStatus::Disabled
        );
        assert!(storage.latest_usage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn refreshes_accounts_concurrently() {
        let storage = test_storage();
        let coordinator = RefreshCoordinator::new(
            storage,
            vec![Arc::new(ConcurrentAccountProvider {
                barrier: Arc::new(Barrier::new(2)),
            })],
        );

        let report = timeout(Duration::from_secs(1), coordinator.refresh(None))
            .await
            .expect("account refreshes should reach the barrier concurrently");

        assert_eq!(report.provider_results.len(), 2);
    }

    #[tokio::test]
    async fn refreshes_providers_concurrently() {
        let storage = test_storage();
        let barrier = Arc::new(Barrier::new(2));
        let coordinator = RefreshCoordinator::new(
            storage,
            vec![
                Arc::new(ConcurrentDiscoveryProvider {
                    provider_id: "codex",
                    barrier: barrier.clone(),
                }),
                Arc::new(ConcurrentDiscoveryProvider {
                    provider_id: "claude",
                    barrier,
                }),
            ],
        );

        let report = timeout(Duration::from_secs(1), coordinator.refresh(None))
            .await
            .expect("provider refreshes should reach the barrier concurrently");

        assert_eq!(report.provider_results.len(), 2);
    }

    #[tokio::test]
    async fn account_collection_returns_when_its_budget_expires() {
        let storage = test_storage();
        let provider_id = ProviderId::new("codex");
        let provider = BlockingProvider {
            provider_id: "codex",
            attempts: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let coordinator = RefreshCoordinator::new(storage, Vec::new());
        let discovered = DiscoveredAccount {
            external_account_id: "timed-out-account".to_string(),
            display_name: None,
            email: None,
            profile_id: Some("default".to_string()),
        };

        let result = timeout(
            Duration::from_secs(1),
            coordinator.refresh_account_with_budget(
                &provider,
                provider_id,
                discovered,
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("the timed-out collector must not be awaited again");

        assert_eq!(result.status, ProviderRefreshStatus::ProviderUnavailable);
        assert_eq!(
            result.message.as_deref(),
            Some("Usage refresh took too long. Try again.")
        );
    }

    #[tokio::test]
    async fn refresh_updates_email_without_clobbering_a_user_name() {
        let storage = test_storage();
        let coordinator = RefreshCoordinator::new(storage.clone(), vec![Arc::new(FakeProvider)]);
        coordinator.refresh(None).await;
        let account = storage.accounts().await.unwrap().remove(0);
        storage
            .update_account(&account.id, Some("My Claude"), None, None)
            .await
            .unwrap();

        coordinator.refresh(None).await;

        let account = storage.account(&account.id).await.unwrap().unwrap();
        assert_eq!(account.display_name.as_deref(), Some("My Claude"));
        assert_eq!(account.email.as_deref(), Some("claude@example.com"));
        assert_eq!(
            account.display_name_source,
            usage_core::AccountDisplayNameSource::User
        );
    }

    #[tokio::test]
    async fn rate_limit_backoff_skips_repeated_provider_calls() {
        let storage = test_storage();
        let attempts = Arc::new(AtomicUsize::new(0));
        let coordinator = RefreshCoordinator::new(
            storage.clone(),
            vec![Arc::new(RateLimitedProvider {
                attempts: attempts.clone(),
            })],
        );

        let first = coordinator.refresh(None).await;
        // A fresh coordinator proves the backoff survives daemon/coordinator
        // reconstruction instead of living only in process memory.
        let restarted = RefreshCoordinator::new(
            storage.clone(),
            vec![Arc::new(RateLimitedProvider {
                attempts: attempts.clone(),
            })],
        );
        let second = restarted.refresh(None).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            first.provider_results[0].status,
            ProviderRefreshStatus::RateLimited
        );
        assert_eq!(
            second.provider_results[0].status,
            ProviderRefreshStatus::RateLimited
        );
        assert!(second.provider_results[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("retrying after")));
        let health = storage.provider_health().await.unwrap();
        assert_eq!(health.len(), 1);
        assert!(matches!(health[0].status, ProviderHealthStatus::BackingOff));
    }

    #[tokio::test]
    async fn rate_limit_backoff_increases_and_caps_at_one_hour() {
        let storage = test_storage();
        let provider_id = ProviderId::new("claude");
        let account_id = storage
            .upsert_account(&provider_id, "account", Some("default"), None, None)
            .await
            .unwrap()
            .id;
        let coordinator = RefreshCoordinator::new(storage, Vec::new());
        let now = Utc::now();

        for base_seconds in RATE_LIMIT_BACKOFF_SECONDS {
            let backoff = coordinator
                .note_rate_limit(&provider_id, &account_id, now, "rate limited", None)
                .await;
            let actual = (backoff.retry_at - now).num_seconds();
            assert!(actual >= base_seconds * 8 / 10);
            assert!(actual <= (base_seconds * 12 / 10).min(60 * 60));
        }
        let capped = coordinator
            .note_rate_limit(&provider_id, &account_id, now, "rate limited", None)
            .await;
        assert!((capped.retry_at - now) <= chrono::TimeDelta::hours(1));

        coordinator
            .clear_rate_limit_backoff(&provider_id, &account_id)
            .await;
        let reset = coordinator
            .note_rate_limit(
                &provider_id,
                &account_id,
                now,
                "rate limited",
                Some(now + chrono::TimeDelta::hours(2)),
            )
            .await;
        assert_eq!(reset.retry_at - now, chrono::TimeDelta::hours(2));
    }

    fn test_storage() -> Storage {
        let path = std::env::temp_dir().join(format!("usage-polling-{}.sqlite3", Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
