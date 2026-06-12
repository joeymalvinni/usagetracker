use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use usage_core::{AccountId, ProviderId, ProviderRefreshResult};

use crate::{
    health,
    providers::{ProviderCollector, ProviderError},
    storage::Storage,
};

pub struct RefreshCoordinator {
    storage: Storage,
    providers: Vec<Arc<dyn ProviderCollector>>,
    refresh_lock: Mutex<()>,
}

impl RefreshCoordinator {
    pub fn new(storage: Storage, providers: Vec<Arc<dyn ProviderCollector>>) -> Self {
        Self {
            storage,
            providers,
            refresh_lock: Mutex::new(()),
        }
    }

    pub async fn refresh(&self, filter: Option<&[ProviderId]>) -> RefreshReport {
        let _guard = self.refresh_lock.lock().await;
        let started_at = Utc::now();
        let mut provider_results = Vec::new();
        let filter_values = filter
            .map(|ids| ids.iter().map(ProviderId::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        info!(
            provider_filter = ?filter_values,
            provider_count = self.providers.len(),
            "refresh started"
        );

        for provider in &self.providers {
            let provider_id = provider.provider_id();
            if filter.is_some_and(|filter| !filter.iter().any(|id| id == &provider_id)) {
                debug!(
                    provider_id = provider_id.as_str(),
                    "skipping provider outside refresh filter"
                );
                continue;
            }

            debug!(
                provider_id = provider_id.as_str(),
                "discovering provider accounts"
            );
            match provider.discover_accounts().await {
                Ok(accounts) => {
                    info!(
                        provider_id = provider_id.as_str(),
                        account_count = accounts.len(),
                        "provider account discovery completed"
                    );
                    for discovered in accounts {
                        let account = match self.storage.upsert_account(
                            &provider_id,
                            &discovered.external_account_id,
                            discovered.display_name.as_deref(),
                        ) {
                            Ok(account) => account,
                            Err(err) => {
                                warn!(
                                    provider_id = provider_id.as_str(),
                                    error = %err,
                                    "failed to store provider account"
                                );
                                provider_results.push(storage_error_result(
                                    provider_id.clone(),
                                    None,
                                    format!("failed to store account: {err}"),
                                ));
                                continue;
                            }
                        };

                        debug!(
                            provider_id = provider_id.as_str(),
                            account_id = account.id.as_str(),
                            "collecting provider usage"
                        );
                        match provider.collect_usage(&discovered).await {
                            Ok(result) => {
                                let mut snapshot = result.snapshot;
                                snapshot.account_id = account.id.clone();
                                if let Err(err) = self
                                    .storage
                                    .insert_snapshot(&snapshot, result.raw_payload.as_ref())
                                {
                                    warn!(
                                        provider_id = provider_id.as_str(),
                                        account_id = account.id.as_str(),
                                        error = %err,
                                        "failed to store usage snapshot"
                                    );
                                    provider_results.push(storage_error_result(
                                        provider_id.clone(),
                                        Some(account.id.clone()),
                                        format!("failed to store snapshot: {err}"),
                                    ));
                                    continue;
                                }

                                let ok_health = health::ok(
                                    provider_id.clone(),
                                    account.id.clone(),
                                    result.collection_mode.clone(),
                                );
                                if let Err(err) = self.storage.upsert_health(&ok_health) {
                                    tracing::warn!(
                                        provider_id = provider_id.as_str(),
                                        error = %err,
                                        "failed to store provider health"
                                    );
                                }

                                info!(
                                    provider_id = provider_id.as_str(),
                                    account_id = account.id.as_str(),
                                    windows = snapshot.windows.len(),
                                    "provider usage stored"
                                );
                                provider_results.push(ProviderRefreshResult {
                                    provider_id: provider_id.clone(),
                                    account_id: Some(account.id),
                                    status: "ok".to_string(),
                                    collection_mode: Some(result.collection_mode),
                                    collected_at: Some(snapshot.collected_at),
                                    message: result.warnings.first().cloned(),
                                });
                            }
                            Err(err) => {
                                warn!(
                                    provider_id = provider_id.as_str(),
                                    account_id = account.id.as_str(),
                                    error_code = err.kind().as_str(),
                                    error = %err,
                                    "provider usage collection failed"
                                );
                                self.record_provider_error(
                                    provider_id.clone(),
                                    Some(account.id.clone()),
                                    &err,
                                );
                                provider_results.push(provider_error_result(
                                    provider_id.clone(),
                                    Some(account.id),
                                    err,
                                ));
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        provider_id = provider_id.as_str(),
                        error_code = err.kind().as_str(),
                        error = %err,
                        "provider account discovery failed"
                    );
                    self.record_provider_error(provider_id.clone(), None, &err);
                    provider_results.push(provider_error_result(provider_id, None, err));
                }
            }
        }

        info!(
            results = provider_results.len(),
            elapsed_ms = (Utc::now() - started_at).num_milliseconds(),
            "refresh finished"
        );
        RefreshReport {
            started_at,
            finished_at: Utc::now(),
            provider_results,
        }
    }

    fn record_provider_error(
        &self,
        provider_id: ProviderId,
        account_id: Option<AccountId>,
        error: &ProviderError,
    ) {
        let provider_health = health::from_provider_error(provider_id, account_id, error);
        if let Err(err) = self.storage.upsert_health(&provider_health) {
            tracing::warn!(error = %err, "failed to store provider error health");
        }
    }
}

pub struct RefreshReport {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub provider_results: Vec<ProviderRefreshResult>,
}

fn provider_error_result(
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    error: ProviderError,
) -> ProviderRefreshResult {
    ProviderRefreshResult {
        provider_id,
        account_id,
        status: error.kind().as_str().to_string(),
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
        status: "storage_error".to_string(),
        collection_mode: None,
        collected_at: None,
        message: Some(message),
    }
}
