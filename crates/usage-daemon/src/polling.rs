use std::{sync::Arc, time::Instant};

use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};
use usage_core::{AccountId, ProviderId, ProviderRefreshResult, ProviderRefreshStatus};

use crate::{
    health,
    providers::{
        DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
        ProviderErrorKind,
    },
    storage::Storage,
};

pub struct RefreshCoordinator {
    storage: Storage,
    providers: RwLock<Vec<Arc<dyn ProviderCollector>>>,
    refresh_lock: Mutex<()>,
}

impl RefreshCoordinator {
    pub fn new(storage: Storage, providers: Vec<Arc<dyn ProviderCollector>>) -> Self {
        Self {
            storage,
            providers: RwLock::new(providers),
            refresh_lock: Mutex::new(()),
        }
    }

    pub async fn set_providers(&self, providers: Vec<Arc<dyn ProviderCollector>>) {
        let _guard = self.refresh_lock.lock().await;
        *self.providers.write().await = providers;
    }

    pub async fn provider_ids(&self) -> Vec<ProviderId> {
        self.providers
            .read()
            .await
            .iter()
            .map(|provider| provider.provider_id())
            .collect()
    }

    pub async fn refresh(&self, filter: Option<&[ProviderId]>) -> RefreshReport {
        let lock_started = Instant::now();
        let _guard = self.refresh_lock.lock().await;
        let lock_wait_ms = lock_started.elapsed().as_millis();
        let providers = self.providers.read().await.clone();
        let started_at = Utc::now();
        let filter_values = filter
            .map(|ids| ids.iter().map(ProviderId::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        info!(
            provider_filter = ?filter_values,
            provider_count = providers.len(),
            lock_wait_ms,
            "refresh started"
        );

        let refreshes = providers.into_iter().filter_map(|provider| {
            let provider_id = provider.provider_id();
            if should_refresh_provider(&provider_id, filter) {
                Some(self.refresh_provider(provider, provider_id))
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

        let accounts = match provider.discover_accounts().await {
            Ok(accounts) => accounts,
            Err(err) => return vec![self.record_failure(provider_id, None, err).await],
        };

        info!(
            provider_id = provider_id.as_str(),
            account_count = accounts.len(),
            elapsed_ms = discovery_started.elapsed().as_millis(),
            "provider account discovery completed"
        );

        join_all(accounts.into_iter().map(|discovered| {
            self.refresh_account(provider.as_ref(), provider_id.clone(), discovered)
        }))
        .await
    }

    async fn refresh_account(
        &self,
        provider: &dyn ProviderCollector,
        provider_id: ProviderId,
        discovered: DiscoveredAccount,
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

        debug!(
            provider_id = provider_id.as_str(),
            account_id = account.id.as_str(),
            "collecting provider usage"
        );
        let collect_started = Instant::now();

        match provider.collect_usage(&discovered).await {
            Ok(result) => {
                info!(
                    provider_id = provider_id.as_str(),
                    account_id = account.id.as_str(),
                    collection_mode = result.collection_mode.as_str(),
                    elapsed_ms = collect_started.elapsed().as_millis(),
                    warnings = result.warnings.len(),
                    "provider usage collection completed"
                );
                self.store_success(provider_id, account.id, result).await
            }
            Err(err) => {
                warn!(
                    provider_id = provider_id.as_str(),
                    account_id = account.id.as_str(),
                    elapsed_ms = collect_started.elapsed().as_millis(),
                    error_code = err.kind().as_str(),
                    error = %err,
                    "provider usage collection failed"
                );
                self.record_failure(provider_id, Some(account.id), err)
                    .await
            }
        }
    }

    async fn store_success(
        &self,
        provider_id: ProviderId,
        account_id: AccountId,
        result: ProviderCollectionResult,
    ) -> ProviderRefreshResult {
        let daily_usage_days = result.daily_usage.len();
        let snapshot = result.usage.into_snapshot(account_id.clone());
        if snapshot.provider_id != provider_id {
            warn!(
                provider_id = provider_id.as_str(),
                snapshot_provider_id = snapshot.provider_id.as_str(),
                account_id = account_id.as_str(),
                "provider returned usage for a different provider id"
            );
            return provider_error_result(
                provider_id,
                Some(account_id),
                ProviderError::new(
                    ProviderErrorKind::Parse,
                    "provider usage payload had a mismatched provider id",
                ),
            );
        }

        let store_started = Instant::now();
        let ok_health = health::ok(
            provider_id.clone(),
            account_id.clone(),
            result.collection_mode.clone(),
        );
        if let Err(err) = self
            .storage
            .record_success(
                &snapshot,
                result.raw_payload.as_ref(),
                &result.daily_usage,
                &ok_health,
                result.account_email.as_deref(),
            )
            .await
        {
            warn!(
                provider_id = provider_id.as_str(),
                account_id = account_id.as_str(),
                error = %err,
                "failed to atomically store provider refresh"
            );
            return storage_error_result(
                provider_id,
                Some(account_id),
                format!("failed to store provider refresh: {err}"),
            );
        }

        for warning in &result.warnings {
            warn!(
                provider_id = provider_id.as_str(),
                account_id = account_id.as_str(),
                warning = %warning,
                "provider refresh warning"
            );
        }

        info!(
            provider_id = provider_id.as_str(),
            account_id = account_id.as_str(),
            windows = snapshot.windows.len(),
            daily_usage_days,
            collection_mode = result.collection_mode.as_str(),
            elapsed_ms = store_started.elapsed().as_millis(),
            "provider usage stored"
        );
        ProviderRefreshResult {
            provider_id,
            account_id: Some(account_id),
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

pub struct RefreshReport {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub provider_results: Vec<ProviderRefreshResult>,
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
        status: refresh_status_for_provider_error(error.kind()),
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

fn refresh_status_for_provider_error(kind: ProviderErrorKind) -> ProviderRefreshStatus {
    match kind {
        ProviderErrorKind::CredentialsMissing => ProviderRefreshStatus::CredentialsMissing,
        ProviderErrorKind::CredentialsInvalid => ProviderRefreshStatus::CredentialsInvalid,
        ProviderErrorKind::Unauthorized => ProviderRefreshStatus::Unauthorized,
        ProviderErrorKind::RateLimited => ProviderRefreshStatus::RateLimited,
        ProviderErrorKind::Network => ProviderRefreshStatus::Network,
        ProviderErrorKind::Parse => ProviderRefreshStatus::Parse,
        ProviderErrorKind::ProviderUnavailable => ProviderRefreshStatus::ProviderUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use serde_json::json;
    use std::time::Duration;
    use tokio::{sync::Barrier, time::timeout};
    use usage_core::{
        ProviderHealth, ProviderHealthStatus, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind,
    };
    use uuid::Uuid;

    use crate::providers::{ProviderCollectionResult, ProviderUsage};

    struct FakeProvider;

    #[async_trait]
    impl ProviderCollector for FakeProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("claude")
        }

        async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
            Ok(vec![DiscoveredAccount {
                external_account_id: "external-account".to_string(),
                display_name: None,
                email: None,
                profile_id: None,
            }])
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
                raw_payload: None,
                warnings: vec![],
            })
        }
    }

    struct MultiAccountProvider;

    #[async_trait]
    impl ProviderCollector for MultiAccountProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("codex")
        }

        async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
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
            ])
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
                raw_payload: None,
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

        async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
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
                raw_payload: None,
                warnings: Vec::new(),
            })
        }
    }

    struct ConcurrentDiscoveryProvider {
        provider_id: &'static str,
        barrier: Arc<Barrier>,
    }

    #[async_trait]
    impl ProviderCollector for ConcurrentDiscoveryProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new(self.provider_id)
        }

        async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
            self.barrier.wait().await;
            Ok(vec![DiscoveredAccount {
                external_account_id: self.provider_id.to_string(),
                display_name: None,
                email: None,
                profile_id: Some("default".to_string()),
            }])
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
                raw_payload: None,
                warnings: Vec::new(),
            })
        }
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
    async fn refresh_stores_each_discovered_account() {
        let storage = test_storage();
        let coordinator =
            RefreshCoordinator::new(storage.clone(), vec![Arc::new(MultiAccountProvider)]);

        let report = coordinator.refresh(None).await;

        assert_eq!(report.provider_results.len(), 2);
        assert!(report
            .provider_results
            .iter()
            .all(|result| result.status == ProviderRefreshStatus::Ok));

        let accounts = storage.accounts().await.unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts
            .iter()
            .all(|account| account.external_account_id == "same-openai-account"));
        assert!(accounts
            .iter()
            .any(|account| account.profile_id.as_deref() == Some("personal")));
        assert!(accounts
            .iter()
            .any(|account| account.profile_id.as_deref() == Some("work")));

        let snapshots = storage.latest_usage().await.unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots
                .iter()
                .filter(|snapshot| snapshot.provider_id.as_str() == "codex")
                .count(),
            2
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

    fn test_storage() -> Storage {
        let path = std::env::temp_dir().join(format!("usage-polling-{}.sqlite3", Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
