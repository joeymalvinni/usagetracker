use std::sync::Arc;

use chrono::{DateTime, Utc};
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
        *self.providers.write().await = providers;
    }

    pub async fn refresh(&self, filter: Option<&[ProviderId]>) -> RefreshReport {
        let _guard = self.refresh_lock.lock().await;
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

        let mut provider_results = Vec::new();
        for provider in &providers {
            let provider_id = provider.provider_id();
            if !should_refresh_provider(&provider_id, filter) {
                debug!(
                    provider_id = provider_id.as_str(),
                    "skipping provider outside refresh filter"
                );
                continue;
            }

            provider_results.extend(self.refresh_provider(provider.as_ref(), provider_id).await);
        }

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
        provider: &dyn ProviderCollector,
        provider_id: ProviderId,
    ) -> Vec<ProviderRefreshResult> {
        debug!(
            provider_id = provider_id.as_str(),
            "discovering provider accounts"
        );

        let accounts = match provider.discover_accounts().await {
            Ok(accounts) => accounts,
            Err(err) => return vec![self.record_failure(provider_id, None, err).await],
        };

        info!(
            provider_id = provider_id.as_str(),
            account_count = accounts.len(),
            "provider account discovery completed"
        );

        let mut results = Vec::with_capacity(accounts.len());
        for discovered in accounts {
            results.push(
                self.refresh_account(provider, provider_id.clone(), discovered)
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
        let account = match self
            .storage
            .upsert_account(
                &provider_id,
                &discovered.external_account_id,
                discovered.display_name.as_deref(),
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

        debug!(
            provider_id = provider_id.as_str(),
            account_id = account.id.as_str(),
            "collecting provider usage"
        );

        match provider.collect_usage(&discovered).await {
            Ok(result) => self.store_success(provider_id, account.id, result).await,
            Err(err) => {
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

        if let Err(err) = self
            .storage
            .insert_snapshot(&snapshot, result.raw_payload.as_ref())
            .await
        {
            warn!(
                provider_id = provider_id.as_str(),
                account_id = account_id.as_str(),
                error = %err,
                "failed to store usage snapshot"
            );
            return storage_error_result(
                provider_id,
                Some(account_id),
                format!("failed to store snapshot: {err}"),
            );
        }

        let ok_health = health::ok(
            provider_id.clone(),
            account_id.clone(),
            result.collection_mode.clone(),
        );
        if let Err(err) = self.storage.upsert_health(&ok_health).await {
            warn!(
                provider_id = provider_id.as_str(),
                error = %err,
                "failed to store provider health"
            );
        }
        if let Err(err) = self
            .storage
            .delete_provider_level_health(&provider_id)
            .await
        {
            warn!(
                provider_id = provider_id.as_str(),
                error = %err,
                "failed to clear provider-level health"
            );
        }
        if let Some(display_name) = result.account_display_name.as_deref() {
            if let Err(err) = self
                .storage
                .update_account_display_name(&account_id, display_name)
                .await
            {
                warn!(
                    provider_id = provider_id.as_str(),
                    account_id = account_id.as_str(),
                    error = %err,
                    "failed to update account display name"
                );
            }
        }

        info!(
            provider_id = provider_id.as_str(),
            account_id = account_id.as_str(),
            windows = snapshot.windows.len(),
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
                display_name: Some("Claude".to_string()),
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
                collection_mode: "live".to_string(),
                account_display_name: None,
                raw_payload: None,
                warnings: vec![],
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

    fn test_storage() -> Storage {
        let path = std::env::temp_dir().join(format!("usage-polling-{}.sqlite3", Uuid::new_v4()));
        let storage = Storage::open(&path).unwrap();
        let _ = std::fs::remove_file(path);
        storage
    }
}
