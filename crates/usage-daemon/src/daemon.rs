use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::{fs::FileTypeExt, net::UnixStream as StdUnixStream},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use tokio::sync::{watch, Mutex, RwLock};
use tracing::{info, warn};
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ConfigResponse, NotificationConfig,
    ProviderActionResponse, ProviderId, ProviderSetupResponse, ProviderToggle,
};

#[cfg(test)]
use usage_core::default_app_dir;

use crate::{
    config::Config,
    fixtures::{self, FixtureScenario},
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
    notifications: Arc<NotificationManager>,
    poll_schedule_tx: watch::Sender<PollSchedule>,
    local_log_config_tx: watch::Sender<local_logs::LocalLogConfig>,
    fixture_mode: bool,
}

const HIDDEN_PROVIDER_POLL_SECONDS: u64 = 30 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PollSchedule {
    initial: Vec<ProviderId>,
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
        let mut initial = Vec::new();
        let mut by_interval = BTreeMap::<u64, Vec<ProviderId>>::new();
        for descriptor in descriptors {
            let visible = config.provider_enabled(descriptor.id.as_str());
            if visible {
                initial.push(descriptor.id.clone());
            }
            let requested_interval = if visible {
                config.poll_interval_seconds
            } else {
                HIDDEN_PROVIDER_POLL_SECONDS
            };
            let effective_interval =
                requested_interval.max(descriptor.minimum_refresh_interval_seconds);
            by_interval
                .entry(effective_interval)
                .or_default()
                .push(descriptor.id.clone());
        }
        Self {
            initial,
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
struct ConfigUpdateChanges {
    poll_interval: bool,
    providers: bool,
    notifications: bool,
}

impl ConfigUpdateChanges {
    fn any(self) -> bool {
        self.poll_interval || self.providers || self.notifications
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
        let refresh = Arc::new(RefreshCoordinator::with_notifications(
            storage.clone(),
            Vec::new(),
            notifications,
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

    fn new_with_fixture_mode(
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

    pub(crate) async fn mutate_config<T>(
        &self,
        mutation: impl FnOnce(&mut Config) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let guard = self.config_mutation.lock().await;
        let mut updated = self.config.read().await.clone();
        let result = mutation(&mut updated)?;
        let collectors = self.collectors_for_config(&updated).await?;
        updated.persist()?;
        self.publish_local_log_config(&updated);
        *self.config.write().await = updated.clone();
        self.refresh.set_providers(collectors).await;
        self.poll_schedule_tx
            .send_replace(PollSchedule::from_config(&updated));
        drop(guard);
        Ok(result)
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

        let mutation = self.config_mutation.lock().await;
        let config = self.config.read().await.clone();
        let changes = config_update_changes(
            &config,
            poll_interval_seconds,
            providers.as_ref(),
            notifications.as_ref(),
        );
        if !changes.any() {
            drop(mutation);
            return self.config_response().await;
        }

        let mut updated_config = config.clone();
        updated_config.apply_update(poll_interval_seconds, providers.as_ref(), notifications)?;

        let collectors = if changes.providers {
            Some(self.collectors_for_config(&updated_config).await?)
        } else {
            None
        };
        let notifications_reenabled =
            !config.notifications.enabled && updated_config.notifications.enabled;
        let notifications_disabled =
            config.notifications.enabled && !updated_config.notifications.enabled;
        let threshold_policy_changed = notification_threshold_policy_changed(
            &config.notifications,
            &updated_config.notifications,
        );
        updated_config.persist()?;
        *self.config.write().await = updated_config.clone();
        self.publish_local_log_config(&updated_config);
        if let Some(collectors) = collectors {
            self.refresh.set_providers(collectors).await;
        }
        if notifications_reenabled || threshold_policy_changed {
            self.storage.clear_notification_window_state().await?;
        }
        if notifications_disabled {
            self.storage.clear_pending_notifications().await?;
        }
        if changes.notifications {
            self.notifications
                .set_config(updated_config.notifications.clone());
        }
        if changes.poll_interval || changes.providers {
            let schedule = PollSchedule::from_config(&updated_config);
            self.poll_schedule_tx.send_if_modified(|current| {
                if *current == schedule {
                    false
                } else {
                    *current = schedule;
                    true
                }
            });
        }

        let poll_interval_seconds = updated_config.poll_interval_seconds;
        let enabled_providers = updated_config.enabled_provider_ids();
        info!(
            poll_interval_seconds,
            enabled_providers = ?enabled_providers,
            "daemon config updated"
        );
        drop(mutation);
        self.config_response().await
    }

    pub async fn add_provider_account(
        &self,
        provider_id: ProviderId,
        display_name: Option<String>,
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

        let mutation = self.config_mutation.lock().await;
        let previous_config = self.config.read().await.clone();
        let plan = match adapter.delete_handler() {
            Some(handler) => handler.plan_deletion(&previous_config, &account)?,
            None => {
                crate::runtime::provider_adapter::AccountDeletionPlan::unchanged(&previous_config)
            }
        };
        let previous_collectors = self.collectors_for_config(&previous_config).await?;
        let next_collectors = self.collectors_for_config(&plan.config).await?;

        plan.config.persist()?;
        *self.config.write().await = plan.config.clone();
        self.publish_local_log_config(&plan.config);
        self.refresh.set_providers(next_collectors).await;

        if let Err(delete_error) = self.storage.delete_account(&account_id).await {
            let rollback_result = previous_config.persist();
            self.publish_local_log_config(&previous_config);
            *self.config.write().await = previous_config;
            self.refresh.set_providers(previous_collectors).await;
            drop(mutation);
            if let Err(rollback_error) = rollback_result {
                return Err(delete_error.context(format!(
                    "database deletion failed and config rollback also failed: {rollback_error}"
                )));
            }
            return Err(delete_error.context("database deletion failed; config was rolled back"));
        }
        drop(mutation);

        if let Some(path) = plan.managed_profile_path {
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

    pub async fn repair_provider(
        &self,
        provider_id: ProviderId,
        account_id: Option<AccountId>,
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
            )
            .await
    }

    pub async fn launch_provider_account(
        &self,
        account_id: AccountId,
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
        handler
            .launch(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account,
            )
            .await
    }

    fn publish_local_log_config(&self, config: &Config) {
        self.local_log_config_tx
            .send_replace(local_logs::LocalLogConfig::from_config(config));
    }
}

fn config_update_changes(
    config: &Config,
    poll_interval_seconds: Option<u64>,
    providers: Option<&BTreeMap<String, ProviderToggle>>,
    notifications: Option<&NotificationConfig>,
) -> ConfigUpdateChanges {
    ConfigUpdateChanges {
        poll_interval: poll_interval_seconds
            .is_some_and(|seconds| seconds != config.poll_interval_seconds),
        providers: providers.is_some_and(|providers| {
            providers
                .iter()
                .any(|(id, toggle)| config.provider_enabled(id) != toggle.enabled)
        }),
        notifications: notifications
            .is_some_and(|notifications| notifications != &config.notifications),
    }
}

fn notification_threshold_policy_changed(
    previous: &NotificationConfig,
    updated: &NotificationConfig,
) -> bool {
    previous.thresholds_percent_remaining != updated.thresholds_percent_remaining
        || previous
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
            .ne(updated
                .rules
                .iter()
                .filter(|rule| rule.thresholds_percent_remaining.is_some())
                .map(|rule| {
                    (
                        rule.account_id.as_ref(),
                        rule.window_id.as_deref(),
                        rule.thresholds_percent_remaining.as_deref(),
                    )
                }))
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
        if !schedule.initial.is_empty() {
            let report = refresh.refresh(Some(&schedule.initial)).await;
            info!(
                results = report.provider_results.len(),
                "initial visible refresh completed"
            );
        }
        let mut due = poll_deadlines(&schedule, poll_delay);

        loop {
            let next_due =
                due.iter().map(|(_, due)| *due).min().unwrap_or_else(|| {
                    tokio::time::Instant::now() + Duration::from_secs(24 * 60 * 60)
                });
            tokio::select! {
                _ = tokio::time::sleep_until(next_due), if !due.is_empty() => {
                    let now = tokio::time::Instant::now();
                    let mut providers = Vec::new();
                    let mut elapsed_groups = Vec::new();
                    for (index, (group, group_due)) in due.iter().enumerate() {
                        if *group_due <= now {
                            providers.extend(group.providers.iter().cloned());
                            elapsed_groups.push(index);
                        }
                    }
                    let report = refresh.refresh(Some(&providers)).await;
                    let completed_at = tokio::time::Instant::now();
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

fn poll_deadlines(
    schedule: &PollSchedule,
    poll_delay: fn(u64) -> Duration,
) -> Vec<(PollGroup, tokio::time::Instant)> {
    let now = tokio::time::Instant::now();
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    #[test]
    fn classifies_config_updates_by_actual_side_effects() {
        let config = test_config(Path::new("/tmp/unused-usage-config"));
        let unchanged_providers = BTreeMap::from([(
            CODEX_PROVIDER_ID.to_string(),
            ProviderToggle { enabled: true },
        )]);

        assert_eq!(
            config_update_changes(
                &config,
                Some(config.poll_interval_seconds),
                Some(&unchanged_providers),
                Some(&config.notifications),
            ),
            ConfigUpdateChanges::default()
        );
        assert_eq!(
            config_update_changes(&config, Some(60), None, None),
            ConfigUpdateChanges {
                poll_interval: true,
                ..ConfigUpdateChanges::default()
            }
        );
        let disabled_notifications = NotificationConfig {
            enabled: false,
            ..NotificationConfig::default()
        };
        assert_eq!(
            config_update_changes(&config, None, None, Some(&disabled_notifications),),
            ConfigUpdateChanges {
                notifications: true,
                ..ConfigUpdateChanges::default()
            }
        );
        let changed_providers = BTreeMap::from([(
            CLAUDE_PROVIDER_ID.to_string(),
            ProviderToggle { enabled: true },
        )]);
        assert_eq!(
            config_update_changes(&config, None, Some(&changed_providers), None),
            ConfigUpdateChanges {
                providers: true,
                ..ConfigUpdateChanges::default()
            }
        );
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
    async fn first_poll_delay_begins_after_initial_refresh_finishes() {
        let root =
            std::env::temp_dir().join(format!("usage-poll-schedule-test-{}", uuid::Uuid::new_v4()));
        let storage = test_storage_at(&root);
        let provider = Arc::new(BlockingDiscoveryProvider::default());
        let refresh = Arc::new(RefreshCoordinator::new(
            storage.clone(),
            vec![provider.clone()],
        ));
        let (_interval_tx, interval_rx) = watch::channel(PollSchedule {
            initial: vec![ProviderId::new(CODEX_PROVIDER_ID)],
            groups: vec![PollGroup {
                providers: vec![ProviderId::new(CODEX_PROVIDER_ID)],
                interval_seconds: 30,
            }],
        });
        let poll_task =
            spawn_polling_loop_with_delay(interval_rx, refresh.clone(), Duration::from_millis);
        timeout(Duration::from_secs(1), provider.first_started.notified())
            .await
            .expect("the initial refresh should start");

        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);

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
    fn poll_schedule_assigns_hidden_providers_to_the_slow_scope() {
        let root =
            std::env::temp_dir().join(format!("usage-poll-scope-test-{}", uuid::Uuid::new_v4()));
        let config = test_config(&root);
        let schedule = PollSchedule::from_config(&config);

        assert_eq!(schedule.initial, vec![ProviderId::new(CODEX_PROVIDER_ID)]);
        let visible = schedule
            .groups
            .iter()
            .find(|group| group.interval_seconds == config.poll_interval_seconds)
            .unwrap();
        assert_eq!(visible.providers, [ProviderId::new(CODEX_PROVIDER_ID)]);
        let hidden = schedule
            .groups
            .iter()
            .find(|group| group.interval_seconds == HIDDEN_PROVIDER_POLL_SECONDS)
            .unwrap();
        assert!(hidden
            .providers
            .contains(&ProviderId::new(CLAUDE_PROVIDER_ID)));
        assert!(hidden
            .providers
            .contains(&ProviderId::new(OPENCODE_GO_PROVIDER_ID)));
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
            capabilities: usage_core::ProviderCapabilities::default(),
        }];

        let schedule = PollSchedule::from_descriptors(&config, &descriptors);

        assert_eq!(schedule.initial, [ProviderId::new("future_provider")]);
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
        let contents = launchers::claude_launcher_contents(Some(Path::new("/tmp/Claude's Work")));

        assert!(contents.contains("unset CLAUDE_SECURESTORAGE_CONFIG_DIR"));
        assert!(contents.contains("export CLAUDE_CONFIG_DIR='/tmp/Claude'\"'\"'s Work'"));
        assert!(contents.ends_with("exec claude\n"));
    }

    #[test]
    fn legacy_claude_launcher_clears_profile_overrides() {
        let contents = launchers::claude_launcher_contents(None);

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
}
