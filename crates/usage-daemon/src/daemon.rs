use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::{fs::FileTypeExt, net::UnixStream as StdUnixStream},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use tokio::sync::{watch, Mutex, RwLock};
use tracing::{info, warn};
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ConfigResponse, NotificationConfig,
    ProviderActionResponse, ProviderId, ProviderSetupResponse, ProviderSignInAction,
    ProviderToggle,
};

#[cfg(test)]
use usage_core::default_app_dir;

use crate::{
    config::Config,
    connectivity::ConnectivityMonitor,
    fixtures::{self, FixtureScenario},
    import_jobs::ImportJobs,
    local_logs,
    notifications::NotificationManager,
    polling::RefreshCoordinator,
    providers::ProviderCollector,
    runtime::{managed_profiles, provider_registry},
    server::SocketServer,
    storage::Storage,
};

pub struct Daemon {
    runtime: Arc<DaemonRuntime>,
    poll_schedule_rx: watch::Receiver<PollSchedule>,
}

pub struct DaemonRuntime {
    config: RwLock<Config>,
    config_mutation: Mutex<()>,
    pub storage: Storage,
    pub refresh: Arc<RefreshCoordinator>,
    pub import_jobs: Arc<ImportJobs>,
    notifications: Arc<NotificationManager>,
    poll_schedule_tx: watch::Sender<PollSchedule>,
    local_log_config_tx: watch::Sender<local_logs::LocalLogConfig>,
    fixture_mode: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PollSchedule {
    groups: Vec<PollGroup>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PollGroup {
    providers: Vec<ProviderId>,
    interval_seconds: u64,
}

impl PollSchedule {
    fn from_config(config: &Config) -> Self {
        Self::from_descriptors(config, &provider_registry::descriptors())
    }

    fn from_descriptors(config: &Config, descriptors: &[usage_core::ProviderDescriptor]) -> Self {
        let mut by_interval = BTreeMap::<u64, Vec<ProviderId>>::new();
        for descriptor in descriptors {
            if !config.provider_enabled(descriptor.id.as_str()) {
                continue;
            }
            let effective_interval = config
                .poll_interval_seconds
                .max(descriptor.minimum_refresh_interval_seconds);
            by_interval
                .entry(effective_interval)
                .or_default()
                .push(descriptor.id.clone());
        }
        Self {
            groups: by_interval
                .into_iter()
                .map(|(interval_seconds, providers)| PollGroup {
                    providers,
                    interval_seconds,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ConfigChangeSet {
    poll_schedule: bool,
    providers: bool,
    notifications: bool,
}

impl ConfigChangeSet {
    fn between(previous: &Config, updated: &Config) -> Self {
        Self {
            poll_schedule: previous.poll_interval_seconds != updated.poll_interval_seconds
                || previous.providers != updated.providers,
            providers: previous.providers != updated.providers,
            notifications: previous.notifications != updated.notifications,
        }
    }

    fn any(self) -> bool {
        self.poll_schedule || self.providers || self.notifications
    }
}

impl Daemon {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let storage = Storage::open(&config.paths.db)?;
        let providers = provider_registry::build_collectors(&config)?;
        let notifications = NotificationManager::new(storage.clone(), config.notifications.clone());
        let refresh = Arc::new(RefreshCoordinator::with_notifications(
            storage.clone(),
            providers,
            notifications.clone(),
        ));
        let (runtime, poll_schedule_rx) = DaemonRuntime::new(config, storage, refresh);

        Ok(Self {
            runtime,
            poll_schedule_rx,
        })
    }

    pub async fn new_fixture(config: Config, scenario: FixtureScenario) -> anyhow::Result<Self> {
        let storage = Storage::open(&config.paths.db)?;
        fixtures::seed(&storage, scenario).await?;
        let notifications = NotificationManager::new(storage.clone(), true);
        let refresh = Arc::new(RefreshCoordinator::with_notifications_and_connectivity(
            storage.clone(),
            Vec::new(),
            notifications,
            ConnectivityMonitor::fixed(usage_core::ConnectivityStatus::Online),
        ));
        let (runtime, poll_schedule_rx) =
            DaemonRuntime::new_with_fixture_mode(config, storage, refresh, true);
        Ok(Self {
            runtime,
            poll_schedule_rx,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let socket_path = self.runtime.config.read().await.paths.socket.clone();
        prepare_socket_path(&socket_path)?;

        let server = SocketServer::new(self.runtime.clone());
        let listener = SocketServer::bind(&socket_path)?;
        let mut server_task = {
            let socket_path = socket_path.clone();
            tokio::spawn(async move { server.serve(listener, &socket_path).await })
        };

        let mut poll_task = spawn_polling_loop(self.poll_schedule_rx, self.runtime.refresh.clone());
        let local_log_task = if self.runtime.fixture_mode {
            None
        } else {
            Some(local_logs::spawn_change_log_loop(
                self.runtime.refresh.clone(),
                self.runtime.local_log_config_tx.subscribe(),
            ))
        };

        let outcome = tokio::select! {
            signal = shutdown_signal() => {
                signal?;
                info!("shutdown signal received");
                Ok(())
            }
            result = &mut server_task => {
                match result {
                    Ok(Ok(())) => Err(anyhow::anyhow!("daemon socket server stopped unexpectedly")),
                    Ok(Err(err)) => Err(err.context("daemon socket server failed")),
                    Err(err) => Err(anyhow::anyhow!("daemon socket server task failed: {err}")),
                }
            }
            result = &mut poll_task => {
                match result {
                    Ok(()) => Err(anyhow::anyhow!("daemon polling loop stopped unexpectedly")),
                    Err(err) => Err(anyhow::anyhow!("daemon polling task failed: {err}")),
                }
            }
        };

        server_task.abort();
        poll_task.abort();
        if let Some(task) = &local_log_task {
            task.abort();
        }
        let _ = server_task.await;
        let _ = poll_task.await;
        if let Some(task) = local_log_task {
            let _ = task.await;
        }
        crate::providers::launchers::cancel_all_logins();
        if let Err(err) = std::fs::remove_file(&socket_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(error = %err, "failed to remove socket file");
            }
        }
        outcome
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

impl DaemonRuntime {
    pub fn new(
        config: Config,
        storage: Storage,
        refresh: Arc<RefreshCoordinator>,
    ) -> (Arc<Self>, watch::Receiver<PollSchedule>) {
        Self::new_with_fixture_mode(config, storage, refresh, false)
    }

    pub(crate) fn new_with_fixture_mode(
        config: Config,
        storage: Storage,
        refresh: Arc<RefreshCoordinator>,
        fixture_mode: bool,
    ) -> (Arc<Self>, watch::Receiver<PollSchedule>) {
        let notifications = refresh.notification_manager();
        notifications.set_config(config.notifications.clone());
        let (poll_schedule_tx, poll_schedule_rx) =
            watch::channel(PollSchedule::from_config(&config));
        let (local_log_config_tx, _) =
            watch::channel(local_logs::LocalLogConfig::from_config(&config));
        let runtime = Arc::new(Self {
            config: RwLock::new(config),
            config_mutation: Mutex::new(()),
            storage,
            refresh,
            import_jobs: Arc::new(ImportJobs::new()),
            notifications,
            poll_schedule_tx,
            local_log_config_tx,
            fixture_mode,
        });
        (runtime, poll_schedule_rx)
    }

    pub async fn config_response(&self) -> anyhow::Result<ConfigResponse> {
        let data_provider_ids = self
            .storage
            .provider_data_ids()
            .await?
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect();
        Ok(self
            .config_response_for_provider_data(data_provider_ids)
            .await
            .0)
    }

    pub(crate) async fn config_snapshot(&self) -> Config {
        self.config.read().await.clone()
    }

    /// Applies `mutation` to the live configuration and republishes each
    /// affected subscription. This is the single write path for configuration:
    /// account edits, `update_config`, and `delete_account` all funnel through
    /// it so persistence and derived runtime state cannot drift between callers.
    pub(crate) async fn mutate_config<T>(
        &self,
        mutation: impl FnOnce(&mut Config) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.commit_config_change(mutation, || std::future::ready(Ok(())))
            .await
    }

    /// Like [`Self::mutate_config`], but runs `commit` once the new config is
    /// live. If `commit` fails and the config file can be restored, the previous
    /// in-memory copy and derived subscriptions are restored with it.
    async fn commit_config_change<T, C, Fut>(
        &self,
        mutation: impl FnOnce(&mut Config) -> anyhow::Result<T>,
        commit: C,
    ) -> anyhow::Result<T>
    where
        C: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let guard = self.config_mutation.lock().await;
        let previous = self.config.read().await.clone();
        let mut updated = previous.clone();
        let result = mutation(&mut updated)?;

        let changes = ConfigChangeSet::between(&previous, &updated);
        if !changes.any() {
            commit().await?;
            drop(guard);
            return Ok(result);
        }

        // Build both provider sets before swapping so a rollback can restore
        // the previous collectors without a second fallible build. Non-provider
        // edits leave the active collectors untouched.
        let (previous_collectors, updated_collectors) = if changes.providers {
            (
                Some(self.collectors_for_config(&previous).await?),
                Some(self.collectors_for_config(&updated).await?),
            )
        } else {
            (None, None)
        };

        updated.persist()?;
        self.install_config(&updated, updated_collectors, changes)
            .await;
        if let Err(error) = self
            .apply_notification_effects(&previous.notifications, &updated.notifications)
            .await
        {
            let error = self
                .rollback_config_change(
                    &previous,
                    previous_collectors,
                    changes,
                    error,
                    "notification state update failed; config change was rolled back",
                )
                .await;
            drop(guard);
            return Err(error);
        }

        if let Err(commit_error) = commit().await {
            let error = self
                .rollback_config_change(
                    &previous,
                    previous_collectors,
                    changes,
                    commit_error,
                    "config change was rolled back",
                )
                .await;
            drop(guard);
            return Err(error);
        }

        drop(guard);
        Ok(result)
    }

    async fn rollback_config_change(
        &self,
        previous: &Config,
        previous_collectors: Option<Vec<Arc<dyn ProviderCollector>>>,
        changes: ConfigChangeSet,
        error: anyhow::Error,
        context: &str,
    ) -> anyhow::Error {
        match previous.persist() {
            Ok(()) => {
                self.install_config(previous, previous_collectors, changes)
                    .await;
                if changes.notifications {
                    self.notifications
                        .set_config(previous.notifications.clone());
                }
                error.context(context.to_string())
            }
            Err(rollback_error) => error.context(format!(
                "{context}; config file rollback also failed: {rollback_error}"
            )),
        }
    }

    /// Publishes a configuration to the affected live subscribers. The
    /// schedule only fires when its derived value actually changes, so edits
    /// that leave polling untouched don't reset poll deadlines.
    async fn install_config(
        &self,
        config: &Config,
        collectors: Option<Vec<Arc<dyn ProviderCollector>>>,
        changes: ConfigChangeSet,
    ) {
        if changes.providers {
            self.publish_local_log_config(config);
        }
        *self.config.write().await = config.clone();
        if let Some(collectors) = collectors {
            self.refresh.set_providers(collectors).await;
        }
        if changes.poll_schedule {
            self.poll_schedule_tx.send_if_modified(|current| {
                let next = PollSchedule::from_config(config);
                let changed = *current != next;
                if changed {
                    *current = next;
                }
                changed
            });
        }
    }

    /// Reconciles notification state against a configuration change. All of the
    /// effects are derived from the before/after notification config, so every
    /// caller of [`Self::mutate_config`] gets them for free and none can forget
    /// one.
    async fn apply_notification_effects(
        &self,
        previous: &NotificationConfig,
        updated: &NotificationConfig,
    ) -> anyhow::Result<()> {
        if previous == updated {
            return Ok(());
        }
        let reenabled = !previous.enabled && updated.enabled;
        let disabled = previous.enabled && !updated.enabled;
        if reenabled || notification_threshold_policy_changed(previous, updated) {
            self.storage.clear_notification_window_state().await?;
        }
        if disabled {
            self.storage.clear_pending_notifications().await?;
        }
        self.notifications.set_config(updated.clone());
        Ok(())
    }

    pub async fn config_response_for_provider_data(
        &self,
        mut data_provider_ids: BTreeSet<String>,
    ) -> (ConfigResponse, BTreeSet<String>) {
        let config = self.config.read().await;
        data_provider_ids.extend(
            config
                .enabled_provider_ids()
                .into_iter()
                .map(|id| id.as_str().to_string()),
        );
        let response = config.response_with_visible_providers(Some(&data_provider_ids));
        (response, data_provider_ids)
    }

    pub async fn visible_provider_ids(&self) -> anyhow::Result<BTreeSet<String>> {
        let providers = self
            .storage
            .provider_data_ids()
            .await?
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        Ok(self.config_response_for_provider_data(providers).await.1)
    }

    async fn collectors_for_config(
        &self,
        config: &Config,
    ) -> anyhow::Result<Vec<Arc<dyn ProviderCollector>>> {
        if self.fixture_mode {
            Ok(Vec::new())
        } else {
            provider_registry::build_collectors(config)
        }
    }

    pub async fn update_config(
        &self,
        poll_interval_seconds: Option<u64>,
        providers: Option<BTreeMap<String, ProviderToggle>>,
        notifications: Option<NotificationConfig>,
    ) -> anyhow::Result<ConfigResponse> {
        if let Some(providers) = &providers {
            for id in providers.keys() {
                if !provider_registry::is_supported(id) {
                    anyhow::bail!("unknown provider: {id}");
                }
            }
        }

        self.mutate_config(|config| {
            config.apply_update(poll_interval_seconds, providers.as_ref(), notifications)
        })
        .await?;

        let config = self.config.read().await;
        info!(
            poll_interval_seconds = config.poll_interval_seconds,
            enabled_providers = ?config.enabled_provider_ids(),
            "daemon config updated"
        );
        drop(config);
        self.config_response().await
    }

    pub async fn add_provider_account(
        &self,
        provider_id: ProviderId,
        display_name: Option<String>,
        sign_in_action: ProviderSignInAction,
    ) -> anyhow::Result<AddProviderAccountResponse> {
        if self.fixture_mode {
            anyhow::bail!("account sign-in is unavailable in development fixture mode");
        }
        let display_name = display_name
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let adapter = provider_registry::adapter(&provider_id)?;
        let handler = adapter
            .add_account_handler()
            .ok_or_else(|| anyhow::anyhow!("adding accounts is not supported for {provider_id}"))?;
        handler
            .add_account(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                display_name,
                sign_in_action,
            )
            .await
    }

    pub async fn update_account(
        &self,
        account_id: AccountId,
        display_name: Option<String>,
        hidden: Option<bool>,
        collection_enabled: Option<bool>,
    ) -> anyhow::Result<Account> {
        self.storage
            .update_account(
                &account_id,
                display_name.as_deref(),
                hidden,
                collection_enabled,
            )
            .await
    }

    pub async fn remove_account(&self, account_id: AccountId) -> anyhow::Result<Account> {
        self.update_account(account_id, None, Some(true), Some(false))
            .await
    }

    pub async fn delete_account(&self, account_id: AccountId) -> anyhow::Result<AccountId> {
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        if let Some(handler) = adapter.delete_handler() {
            handler.cleanup_before_delete(&account).await?;
        }

        // The database row is deleted as the guarded commit so a failure there
        // rolls the tombstoned config back to exactly what is still on disk.
        let managed_profile_path = self
            .commit_config_change(
                |config| {
                    let plan = match adapter.delete_handler() {
                        Some(handler) => handler.plan_deletion(config, &account)?,
                        None => {
                            crate::runtime::provider_adapter::AccountDeletionPlan::unchanged(config)
                        }
                    };
                    *config = plan.config;
                    Ok(plan.managed_profile_path)
                },
                || async {
                    self.storage
                        .delete_account(&account_id)
                        .await
                        .map(|_| ())
                        .context("database deletion failed")
                },
            )
            .await?;

        if let Some(path) = managed_profile_path {
            managed_profiles::quarantine_and_remove(&path)?;
        }
        Ok(account_id)
    }

    pub async fn provider_setup(
        &self,
        provider_id: ProviderId,
    ) -> anyhow::Result<ProviderSetupResponse> {
        let adapter = provider_registry::adapter(&provider_id)?;
        if let Some(handler) = adapter.setup_handler() {
            handler
                .get_setup(crate::runtime::provider_adapter::ProviderRuntime::new(self))
                .await
        } else {
            let config = self.config.read().await;
            let provider_config = config
                .providers
                .get(provider_id.as_str())
                .cloned()
                .unwrap_or_default();
            Ok(adapter.setup_summary(&provider_config))
        }
    }

    pub async fn update_provider_setup(
        &self,
        provider_id: ProviderId,
        settings: BTreeMap<String, Option<String>>,
    ) -> anyhow::Result<ProviderSetupResponse> {
        let adapter = provider_registry::adapter(&provider_id)?;
        let handler = adapter
            .setup_handler()
            .ok_or_else(|| anyhow::anyhow!("setup is not supported for {provider_id}"))?;
        handler
            .update_setup(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                settings,
            )
            .await
    }

    pub async fn request_credential_access(
        &self,
        provider_id: ProviderId,
        profile_id: Option<String>,
    ) -> anyhow::Result<ProviderActionResponse> {
        anyhow::ensure!(
            !self.fixture_mode,
            "credential access is unavailable in fixture mode"
        );
        let adapter = provider_registry::adapter(&provider_id)?;
        let config = self
            .config
            .read()
            .await
            .providers
            .get(provider_id.as_str())
            .cloned()
            .unwrap_or_default();
        // This also works for a provider paused after failed onboarding. Asking
        // for permission itself never enables polling or starts browser login.
        let collector = adapter.build_collector(&config)?;
        if let Some(id) = &profile_id {
            anyhow::ensure!(
                collector.configured_profile_ids().contains(id),
                "The selected credential profile is unavailable"
            );
        }
        anyhow::ensure!(
            profile_id.is_some() || collector.configured_profile_ids().len() <= 1,
            "Choose an account or pending profile before requesting credential access"
        );
        self.refresh
            .invalidate_cached_credentials(&provider_id, profile_id.as_deref())
            .await?;
        collector
            .request_credential_access(profile_id.as_deref())
            .await?;
        Ok(ProviderActionResponse {
            provider_id,
            message: "Credential access checked.".to_string(),
            authentication_url: None,
        })
    }

    pub async fn repair_provider(
        &self,
        provider_id: ProviderId,
        account_id: Option<AccountId>,
        sign_in_action: ProviderSignInAction,
    ) -> anyhow::Result<ProviderActionResponse> {
        if self.fixture_mode {
            anyhow::bail!("provider repair is unavailable in development fixture mode");
        }
        let adapter = provider_registry::adapter(&provider_id)?;
        let handler = adapter
            .repair_handler()
            .ok_or_else(|| anyhow::anyhow!("repair is not supported for {provider_id}"))?;
        handler
            .repair(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account_id,
                sign_in_action,
            )
            .await
    }

    pub async fn launch_provider_account(
        &self,
        account_id: AccountId,
        overrides: crate::runtime::provider_adapter::LaunchOverrides,
    ) -> anyhow::Result<ProviderActionResponse> {
        if self.fixture_mode {
            anyhow::bail!("provider launch is unavailable in development fixture mode");
        }
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        let handler = adapter.launch_handler().ok_or_else(|| {
            anyhow::anyhow!(
                "profile sessions are not supported for {}",
                account.provider_id
            )
        })?;
        if !handler.supports_launch_options()
            && (overrides.working_directory.is_some() || overrides.launch.is_some())
        {
            anyhow::bail!(
                "launch overrides are not supported for {}",
                account.provider_id
            );
        }
        handler
            .launch(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account,
                overrides,
            )
            .await
    }

    /// Read-only, so fixture mode is allowed — the Open sheet stays demoable
    /// via `just fixture` even though the launch itself is rejected there.
    pub async fn account_launch_settings(
        &self,
        account_id: AccountId,
    ) -> anyhow::Result<usage_core::AccountLaunchSettingsResponse> {
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        let handler = adapter.launch_handler().ok_or_else(|| {
            anyhow::anyhow!(
                "launch settings are not supported for {}",
                account.provider_id
            )
        })?;
        anyhow::ensure!(
            handler.supports_launch_options(),
            "launch settings are not supported for {}",
            account.provider_id
        );
        handler
            .launch_settings(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account,
            )
            .await
    }

    /// Read-only: fixture mode is allowed so `just fixture` can demo the
    /// Import sheet against real `~/.claude` sizes (never mutating anything).
    pub async fn preview_account_import(
        &self,
        account_id: usage_core::AccountId,
    ) -> anyhow::Result<usage_core::AccountImportPreview> {
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        let handler = adapter.import_handler().ok_or_else(|| {
            anyhow::anyhow!(
                "importing account data is not supported for {}",
                account.provider_id
            )
        })?;
        handler
            .preview(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account,
            )
            .await
    }

    pub async fn import_account_data(
        self: &Arc<Self>,
        account_id: usage_core::AccountId,
        options: usage_core::ImportOptions,
        mode: usage_core::ImportMode,
    ) -> anyhow::Result<usage_core::ImportJob> {
        if self.fixture_mode {
            anyhow::bail!("account import is unavailable in development fixture mode");
        }
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        let handler = adapter.import_handler().ok_or_else(|| {
            anyhow::anyhow!(
                "importing account data is not supported for {}",
                account.provider_id
            )
        })?;
        if self
            .import_jobs
            .account_has_active_import(&account_id)
            .await
        {
            anyhow::bail!(
                "an import is already running for account {}",
                account_id.as_str()
            );
        }
        handler
            .start_import(self.clone(), account, options, mode)
            .await
    }

    pub async fn get_import_job(
        &self,
        job_id: &usage_core::ImportJobId,
    ) -> anyhow::Result<Option<usage_core::ImportJob>> {
        Ok(self.import_jobs.get(job_id).await)
    }

    fn publish_local_log_config(&self, config: &Config) {
        self.local_log_config_tx
            .send_replace(local_logs::LocalLogConfig::from_config(config));
    }
}

/// A single rule's positional-notification identity: the account and window it
/// targets and the threshold ladder it overrides.
type ThresholdRule<'a> = (Option<&'a AccountId>, Option<&'a str>, Option<&'a [u8]>);

/// The per-rule threshold overrides that steer positional notification state.
/// Only rules that actually set thresholds participate, so unrelated rule edits
/// don't force the stored notification window state to be cleared.
fn threshold_rules(config: &NotificationConfig) -> Vec<ThresholdRule<'_>> {
    config
        .rules
        .iter()
        .filter(|rule| rule.thresholds_percent_remaining.is_some())
        .map(|rule| {
            (
                rule.account_id.as_ref(),
                rule.window_id.as_deref(),
                rule.thresholds_percent_remaining.as_deref(),
            )
        })
        .collect()
}

fn notification_threshold_policy_changed(
    previous: &NotificationConfig,
    updated: &NotificationConfig,
) -> bool {
    previous.thresholds_percent_remaining != updated.thresholds_percent_remaining
        || threshold_rules(previous) != threshold_rules(updated)
}

fn spawn_polling_loop(
    schedule_rx: watch::Receiver<PollSchedule>,
    refresh: Arc<RefreshCoordinator>,
) -> tokio::task::JoinHandle<()> {
    spawn_polling_loop_with_delay(schedule_rx, refresh, Duration::from_secs)
}

fn spawn_polling_loop_with_delay(
    mut schedule_rx: watch::Receiver<PollSchedule>,
    refresh: Arc<RefreshCoordinator>,
    poll_delay: fn(u64) -> Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut schedule = schedule_rx.borrow_and_update().clone();
        // Startup is deliberately passive. The foreground app performs explicit
        // refreshes after onboarding or when its UI opens, while the daemon waits
        // a full interval before background collection. This prevents a persisted
        // provider configuration from racing first-run consent.
        let mut due = poll_deadlines(&schedule, poll_delay);

        loop {
            let next_due = due
                .iter()
                .map(|(_, due)| *due)
                .min()
                .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(24 * 60 * 60));
            tokio::select! {
                _ = tokio::time::sleep(poll_wait(next_due, SystemTime::now())), if !due.is_empty() => {
                    let now = SystemTime::now();
                    let mut providers = Vec::new();
                    let mut elapsed_groups = Vec::new();
                    for (index, (group, group_due)) in due.iter().enumerate() {
                        if *group_due <= now {
                            providers.extend(group.providers.iter().cloned());
                            elapsed_groups.push(index);
                        }
                    }
                    if providers.is_empty() { continue; }
                    let report = refresh.refresh(Some(&providers)).await;
                    let completed_at = SystemTime::now();
                    for index in elapsed_groups {
                        let (group, group_due) = &mut due[index];
                        *group_due = completed_at + poll_delay(group.interval_seconds);
                    }
                    info!(
                        provider_count = providers.len(),
                        results = report.provider_results.len(),
                        "scheduled provider poll completed"
                    );
                }
                changed = schedule_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    schedule = schedule_rx.borrow_and_update().clone();
                    due = poll_deadlines(&schedule, poll_delay);
                    info!("provider poll schedule changed");
                }
            }
        }
    })
}

// Recheck wall-clock deadlines regularly: elapsed sleep time must count toward
// a provider's interval even on platforms whose async timer pauses in sleep.
fn poll_wait(deadline: SystemTime, now: SystemTime) -> Duration {
    deadline
        .duration_since(now)
        .unwrap_or_default()
        .min(Duration::from_secs(15))
}

fn poll_deadlines(
    schedule: &PollSchedule,
    poll_delay: fn(u64) -> Duration,
) -> Vec<(PollGroup, SystemTime)> {
    let now = SystemTime::now();
    schedule
        .groups
        .iter()
        .cloned()
        .map(|group| {
            let due = now + poll_delay(group.interval_seconds);
            (group, due)
        })
        .collect()
}

fn prepare_socket_path(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }

    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if !metadata.file_type().is_socket() {
        anyhow::bail!(
            "refusing to remove non-socket path {}",
            socket_path.display()
        );
    }

    match StdUnixStream::connect(socket_path) {
        Ok(_) => anyhow::bail!(
            "daemon socket {} is already accepting connections; refusing to replace a live daemon socket",
            socket_path.display()
        ),
        Err(err) => info!(
            socket = %socket_path.display(),
            error = %err,
            "removing stale daemon socket"
        ),
    }
    std::fs::remove_file(socket_path)
        .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderConfig, ProviderProfileConfig};
    use crate::providers::{
        claude::{keychain_service_for_config_dir, PROVIDER_ID as CLAUDE_PROVIDER_ID},
        codex::PROVIDER_ID as CODEX_PROVIDER_ID,
        grok::profile_service::{
            ensure_login_profile as ensure_grok_login_profile,
            select_login_target as select_grok_login_target,
        },
        launchers,
        opencode::OPENCODE_GO_PROVIDER_ID,
        profile_service::{
            ensure_claude_login_profile, pending_codex_profile, push_managed_claude_profile,
            unique_profile_id,
        },
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::{sync::Notify, time::timeout};

    #[derive(Default)]
    struct BlockingDiscoveryProvider {
        attempts: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
        first_finished: Notify,
    }

    #[async_trait]
    impl ProviderCollector for BlockingDiscoveryProvider {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new(CODEX_PROVIDER_ID)
        }

        fn configured_profile_ids(&self) -> Vec<String> {
            Vec::new()
        }

        async fn discover_accounts(
            &self,
        ) -> Result<crate::providers::AccountDiscovery, crate::providers::ProviderError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
                self.first_finished.notify_one();
            }
            Ok(Vec::new().into())
        }

        async fn collect_usage(
            &self,
            _account: &crate::providers::DiscoveredAccount,
        ) -> Result<crate::providers::CollectionOutcome, crate::providers::ProviderError> {
            unreachable!("the blocking test provider discovers no accounts")
        }
    }

    fn test_config(root: &Path) -> Config {
        let providers = BTreeMap::from([
            (
                CODEX_PROVIDER_ID.to_string(),
                ProviderConfig {
                    enabled: true,
                    ..ProviderConfig::default()
                },
            ),
            (CLAUDE_PROVIDER_ID.to_string(), ProviderConfig::default()),
            (
                OPENCODE_GO_PROVIDER_ID.to_string(),
                ProviderConfig::default(),
            ),
        ]);
        Config {
            poll_interval_seconds: 300,
            notifications: NotificationConfig::default(),
            providers,
            paths: crate::config::Paths {
                config: root.join("config.json"),
                db: root.join("usage.sqlite3"),
                socket: root.join("usage.sock"),
            },
        }
    }

    fn test_storage_at(root: &Path) -> Storage {
        std::fs::create_dir_all(root).unwrap();
        Storage::open(&root.join("usage.sqlite3")).unwrap()
    }

    fn short_socket_test_root() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp").join(format!(
            "ut-sock-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ))
    }

    #[test]
    fn stale_socket_cleanup_removes_only_socket_files() {
        let root = short_socket_test_root();
        std::fs::create_dir_all(&root).unwrap();
        let socket_path = root.join("usage.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(listener);

        prepare_socket_path(&socket_path).unwrap();

        assert!(!socket_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_socket_cleanup_preserves_non_socket_paths() {
        let root = short_socket_test_root();
        std::fs::create_dir_all(&root).unwrap();
        let socket_path = root.join("usage.sock");
        std::fs::write(&socket_path, b"keep me").unwrap();

        let error = prepare_socket_path(&socket_path).unwrap_err();

        assert!(error.to_string().contains("refusing to remove non-socket"));
        assert_eq!(std::fs::read(&socket_path).unwrap(), b"keep me");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_socket_cleanup_refuses_a_live_daemon_socket() {
        let root = short_socket_test_root();
        std::fs::create_dir_all(&root).unwrap();
        let socket_path = root.join("usage.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let error = prepare_socket_path(&socket_path).unwrap_err();

        assert!(error.to_string().contains("already accepting connections"));
        assert!(socket_path.exists());
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn config_commit_runs_when_the_mutation_is_a_noop() {
        let root = std::env::temp_dir().join(format!(
            "usage-runtime-noop-commit-test-{}",
            uuid::Uuid::new_v4()
        ));
        let storage = test_storage_at(&root);
        let refresh = Arc::new(RefreshCoordinator::new(storage.clone(), Vec::new()));
        let (runtime, _schedule_rx) =
            DaemonRuntime::new(test_config(&root), storage.clone(), refresh.clone());
        let committed = Arc::new(AtomicBool::new(false));
        let commit_flag = committed.clone();

        runtime
            .commit_config_change(
                |_| Ok(()),
                || async move {
                    commit_flag.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();

        assert!(committed.load(Ordering::SeqCst));
        drop(runtime);
        drop(refresh);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn interval_only_update_does_not_wait_for_an_active_refresh() {
        let root = std::env::temp_dir().join(format!(
            "usage-runtime-config-test-{}",
            uuid::Uuid::new_v4()
        ));
        let storage = test_storage_at(&root);
        let provider = Arc::new(BlockingDiscoveryProvider::default());
        let refresh = Arc::new(RefreshCoordinator::new(
            storage.clone(),
            vec![provider.clone()],
        ));
        let (runtime, mut interval_rx) =
            DaemonRuntime::new(test_config(&root), storage.clone(), refresh.clone());
        let refresh_task = {
            let refresh = refresh.clone();
            tokio::spawn(async move { refresh.refresh(None).await })
        };
        timeout(Duration::from_secs(1), provider.first_started.notified())
            .await
            .expect("the active refresh should start");

        let update = timeout(
            Duration::from_secs(1),
            runtime.update_config(Some(301), None, None),
        )
        .await;
        provider.release_first.notify_one();
        refresh_task.await.unwrap();

        let response = update
            .expect("an interval-only update must not wait for the refresh lock")
            .unwrap();
        assert_eq!(response.poll_interval_seconds, 301);
        assert!(interval_rx.has_changed().unwrap());
        let updated_schedule = interval_rx.borrow_and_update();
        assert!(updated_schedule.groups.iter().any(|group| {
            group.interval_seconds == 301
                && group
                    .providers
                    .contains(&ProviderId::new(CODEX_PROVIDER_ID))
        }));

        drop(runtime);
        drop(refresh);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn threshold_policy_update_clears_positional_notification_state() {
        let root = std::env::temp_dir().join(format!(
            "usage-runtime-notification-test-{}",
            uuid::Uuid::new_v4()
        ));
        let storage = test_storage_at(&root);
        let provider_id = ProviderId::new(CODEX_PROVIDER_ID);
        let account = storage
            .upsert_account(&provider_id, "notification-account", None, None, None)
            .await
            .unwrap();
        storage
            .upsert_notification_window_state(
                &account.id,
                "weekly",
                crate::storage::NotificationWindowState {
                    reset_at: None,
                    notified_mask: 1,
                    last_attempt_at: None,
                },
            )
            .await
            .unwrap();
        let refresh = Arc::new(RefreshCoordinator::new(storage.clone(), Vec::new()));
        let (runtime, _schedule_rx) =
            DaemonRuntime::new(test_config(&root), storage.clone(), refresh);
        let notifications = NotificationConfig {
            thresholds_percent_remaining: vec![25, 10],
            ..NotificationConfig::default()
        };

        runtime
            .update_config(None, None, Some(notifications))
            .await
            .unwrap();

        assert!(storage
            .notification_window_state(&account.id, "weekly")
            .await
            .unwrap()
            .is_none());

        drop(runtime);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn daemon_waits_a_full_interval_before_its_first_refresh() {
        let root =
            std::env::temp_dir().join(format!("usage-poll-schedule-test-{}", uuid::Uuid::new_v4()));
        let storage = test_storage_at(&root);
        let provider = Arc::new(BlockingDiscoveryProvider::default());
        let refresh = Arc::new(RefreshCoordinator::new(
            storage.clone(),
            vec![provider.clone()],
        ));
        let (_interval_tx, interval_rx) = watch::channel(PollSchedule {
            groups: vec![PollGroup {
                providers: vec![ProviderId::new(CODEX_PROVIDER_ID)],
                interval_seconds: 30,
            }],
        });
        let poll_task =
            spawn_polling_loop_with_delay(interval_rx, refresh.clone(), Duration::from_millis);

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(provider.attempts.load(Ordering::SeqCst), 0);
        timeout(Duration::from_secs(1), provider.first_started.notified())
            .await
            .expect("the first periodic refresh should start after the delay");

        provider.release_first.notify_one();
        timeout(Duration::from_secs(1), provider.first_finished.notified())
            .await
            .expect("the initial refresh should finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
        timeout(Duration::from_secs(1), async {
            while provider.attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first periodic refresh should run after the full delay");

        poll_task.abort();
        let _ = poll_task.await;
        drop(refresh);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn poll_deadline_is_due_after_sleep_and_waits_are_bounded() {
        let before_sleep = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let deadline = before_sleep + Duration::from_secs(300);
        assert_eq!(poll_wait(deadline, before_sleep), Duration::from_secs(15));
        assert_eq!(
            poll_wait(deadline, deadline - Duration::from_secs(2)),
            Duration::from_secs(2)
        );
        assert_eq!(poll_wait(deadline, deadline), Duration::ZERO);
        assert_eq!(
            poll_wait(deadline, before_sleep + Duration::from_secs(14 * 3_600)),
            Duration::ZERO
        );
    }

    #[test]
    fn poll_schedule_excludes_disabled_providers() {
        let root =
            std::env::temp_dir().join(format!("usage-poll-scope-test-{}", uuid::Uuid::new_v4()));
        let config = test_config(&root);
        let schedule = PollSchedule::from_config(&config);

        assert_eq!(
            schedule.groups,
            vec![PollGroup {
                providers: vec![ProviderId::new(CODEX_PROVIDER_ID)],
                interval_seconds: config.poll_interval_seconds,
            }]
        );
    }

    #[test]
    fn poll_schedule_honors_a_new_providers_declared_minimum() {
        let root =
            std::env::temp_dir().join(format!("usage-poll-minimum-test-{}", uuid::Uuid::new_v4()));
        let mut config = test_config(&root);
        config.providers.insert(
            "future_provider".to_string(),
            ProviderConfig {
                enabled: true,
                ..ProviderConfig::default()
            },
        );
        config.poll_interval_seconds = 60;
        let descriptors = vec![usage_core::ProviderDescriptor {
            id: ProviderId::new("future_provider"),
            display_name: "Future Provider".to_string(),
            minimum_refresh_interval_seconds: 900,
            detected: false,
            credential_access_notice: None,
            capabilities: usage_core::ProviderCapabilities::default(),
        }];

        let schedule = PollSchedule::from_descriptors(&config, &descriptors);

        assert_eq!(schedule.groups[0].interval_seconds, 900);
    }

    #[test]
    fn profile_id_generation_uses_label_and_avoids_existing_profiles() {
        let profiles = vec![
            ProviderProfileConfig {
                id: Some("default".to_string()),
                ..ProviderProfileConfig::default()
            },
            ProviderProfileConfig {
                id: Some("work".to_string()),
                ..ProviderProfileConfig::default()
            },
        ];

        assert_eq!(unique_profile_id(&profiles, Some("Work")), "work-2");
        assert_eq!(
            unique_profile_id(&profiles, Some("Personal Email")),
            "personal-email"
        );
        assert_eq!(unique_profile_id(&profiles, None), "account");

        let whitespace = vec![ProviderProfileConfig {
            id: Some(" account ".to_string()),
            ..ProviderProfileConfig::default()
        }];
        assert_eq!(unique_profile_id(&whitespace, None), "account-2");
    }

    #[test]
    fn pending_login_is_reused_and_profile_deletion_is_path_safe() {
        let managed_path = default_app_dir()
            .unwrap()
            .join("profiles")
            .join(CODEX_PROVIDER_ID)
            .join("pending-test");
        let mut profile = ProviderProfileConfig {
            id: Some("pending-test".to_string()),
            display_name: Some("Pending".to_string()),
            ..ProviderProfileConfig::default()
        };
        crate::providers::codex::settings::update_profile(&mut profile, |settings| {
            settings.codex_home = Some(managed_path.clone());
        })
        .unwrap();
        let provider = ProviderConfig {
            enabled: true,
            profiles: vec![profile],
            ..ProviderConfig::default()
        };

        let pending = pending_codex_profile(&provider).unwrap();
        assert_eq!(pending.0, "pending-test");
        assert_eq!(pending.1, managed_path);
        assert!(managed_profiles::is_managed_profile(
            &pending.1,
            CODEX_PROVIDER_ID
        ));
        assert!(!managed_profiles::is_managed_profile(
            &dirs::home_dir().unwrap().join(".codex"),
            CODEX_PROVIDER_ID
        ));
    }

    #[test]
    fn grok_pending_login_never_reuses_a_connected_profile() {
        let root = std::env::temp_dir().join(format!("grok-pending-{}", uuid::Uuid::new_v4()));
        let grok_profile = |id: &str, home: std::path::PathBuf| {
            let mut profile = ProviderProfileConfig {
                id: Some(id.to_string()),
                ..ProviderProfileConfig::default()
            };
            crate::providers::grok::settings::update_profile(&mut profile, |settings| {
                settings.grok_home = Some(home);
            })
            .unwrap();
            profile
        };
        let mut provider = ProviderConfig {
            enabled: true,
            profiles: vec![
                grok_profile("default", root.join("default")),
                grok_profile("work", root.join("work")),
            ],
            ..ProviderConfig::default()
        };
        let connected = BTreeSet::from(["default".to_string()]);

        let pending = select_grok_login_target(&mut provider, &connected, None).unwrap();

        assert_eq!(pending.profile_id, "work");
        assert_eq!(pending.grok_home, root.join("work"));
    }

    #[test]
    fn creates_an_isolated_managed_claude_profile() {
        let root = std::env::temp_dir().join(format!("claude-profile-{}", uuid::Uuid::new_v4()));
        let mut provider = ProviderConfig::default();

        let target = push_managed_claude_profile(
            &mut provider,
            "work".to_string(),
            Some("Work".to_string()),
            root.clone(),
        )
        .unwrap();

        assert_eq!(target.profile_id, "work");
        assert_eq!(target.config_dir.as_deref(), Some(root.as_path()));
        let profile = provider.profiles.first().unwrap();
        let settings = crate::providers::claude::settings::profile(profile).unwrap();
        assert!(profile.enabled);
        assert!(!profile.deleted);
        assert_eq!(profile.display_name.as_deref(), Some("Work"));
        assert_eq!(settings.claude_config_dir.as_deref(), Some(root.as_path()));
        assert_eq!(
            settings.keychain_service.as_deref(),
            Some(keychain_service_for_config_dir(&root).as_str())
        );
        assert_eq!(
            settings.credentials_file.as_deref(),
            Some(root.join(".credentials.json").as_path())
        );
        assert_eq!(settings.project_roots, vec![root.join("projects")]);
        assert!(settings.owns_default_claude_activity);
        assert_eq!(settings.cli_enabled, Some(true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repairing_a_legacy_claude_account_keeps_the_default_profile() {
        let mut provider = ProviderConfig::default();

        let target = ensure_claude_login_profile(&mut provider, Some("default")).unwrap();

        assert_eq!(target.profile_id, "default");
        assert!(target.config_dir.is_none());
        assert!(provider.profiles.is_empty());
    }

    #[test]
    fn reconnecting_grok_restores_a_tombstoned_default_profile() {
        let mut provider = ProviderConfig {
            profiles: vec![ProviderProfileConfig {
                id: Some("default".to_string()),
                enabled: false,
                deleted: true,
                ..ProviderProfileConfig::default()
            }],
            ..ProviderConfig::default()
        };

        let target = ensure_grok_login_profile(&mut provider, Some("default")).unwrap();

        assert_eq!(target.profile_id, "default");
        assert!(provider.profiles[0].enabled);
        assert!(!provider.profiles[0].deleted);
        assert!(
            crate::providers::grok::settings::profile(&provider.profiles[0])
                .unwrap()
                .grok_home
                .is_some()
        );
    }

    #[test]
    fn claude_launcher_pins_activity_to_the_profile_config_directory() {
        let contents =
            launchers::claude_launcher_contents(Some(Path::new("/tmp/Claude's Work")), None, None);

        assert!(contents.contains("unset CLAUDE_SECURESTORAGE_CONFIG_DIR"));
        assert!(contents.contains("export CLAUDE_CONFIG_DIR='/tmp/Claude'\"'\"'s Work'"));
        assert!(contents.ends_with("exec claude\n"));
    }

    #[test]
    fn legacy_claude_launcher_clears_profile_overrides() {
        let contents = launchers::claude_launcher_contents(None, None, None);

        assert!(contents.contains("unset CLAUDE_CONFIG_DIR"));
        assert!(!contents.contains("export CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn launch_capabilities_match_the_daemon_handlers() {
        let launchable = provider_registry::descriptors()
            .into_iter()
            .filter(|provider| provider.capabilities.launch_account)
            .map(|provider| provider.id)
            .collect::<Vec<_>>();

        assert_eq!(launchable, vec![ProviderId::new(CLAUDE_PROVIDER_ID)]);
    }

    #[test]
    fn launch_options_capability_is_claude_only() {
        let with_options = provider_registry::descriptors()
            .into_iter()
            .filter(|provider| provider.capabilities.launch_options)
            .map(|provider| provider.id)
            .collect::<Vec<_>>();

        assert_eq!(with_options, vec![ProviderId::new(CLAUDE_PROVIDER_ID)]);
    }

    #[test]
    fn import_capability_is_claude_only() {
        let with_import = provider_registry::descriptors()
            .into_iter()
            .filter(|provider| provider.capabilities.import_account_data)
            .map(|provider| provider.id)
            .collect::<Vec<_>>();

        assert_eq!(with_import, vec![ProviderId::new(CLAUDE_PROVIDER_ID)]);
    }
}
