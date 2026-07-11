use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::net::UnixStream as StdUnixStream,
    path::Path,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{watch, Mutex, RwLock};
use tracing::{info, warn};
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ConfigResponse, NotificationConfig,
    ProviderActionResponse, ProviderId, ProviderProfileResponse, ProviderSetupResponse,
    ProviderToggle,
};

#[cfg(test)]
use usage_core::default_app_dir;

use crate::{
    config::{Config, ProviderConfig, ProviderProfileConfig},
    fixtures::{self, FixtureScenario},
    local_logs,
    notifications::NotificationManager,
    polling::RefreshCoordinator,
    providers::{
        claude::PROVIDER_ID as CLAUDE_PROVIDER_ID,
        codex::PROVIDER_ID as CODEX_PROVIDER_ID,
        opencode::{clear_cached_cookie_cache, OpenCodeCollector, OPENCODE_GO_PROVIDER_ID},
        paths::expand_home_path,
        ProviderCollector,
    },
    runtime::{launchers, managed_profiles, profile_service, provider_registry},
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
    visible: Vec<ProviderId>,
    hidden: Vec<ProviderId>,
    visible_interval_seconds: u64,
}

impl PollSchedule {
    fn from_config(config: &Config) -> Self {
        let visible = config.enabled_provider_ids();
        let visible_ids = visible
            .iter()
            .map(ProviderId::as_str)
            .collect::<BTreeSet<_>>();
        let hidden = config
            .providers
            .keys()
            .filter(|id| provider_registry::is_supported(id))
            .filter(|id| !visible_ids.contains(id.as_str()))
            .map(|id| ProviderId::new(id.clone()))
            .collect();
        Self {
            visible,
            hidden,
            visible_interval_seconds: config.poll_interval_seconds,
        }
    }
}

use profile_service::{
    codex_profile_home, create_managed_claude_profile, default_codex_profile,
    ensure_claude_login_profile, pending_claude_profile, pending_codex_profile, unique_profile_id,
    ClaudeLoginTarget,
};

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
        let providers = provider_registry::build_enabled(&config, &storage).await?;
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
        let visible_providers = self.visible_provider_ids().await?;
        Ok(self
            .config
            .read()
            .await
            .response_with_visible_providers(Some(&visible_providers)))
    }

    pub async fn visible_provider_ids(&self) -> anyhow::Result<BTreeSet<String>> {
        let mut providers = self
            .storage
            .provider_data_ids()
            .await?
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        providers.extend(
            self.config
                .read()
                .await
                .enabled_provider_ids()
                .into_iter()
                .map(|id| id.as_str().to_string()),
        );
        Ok(providers)
    }

    async fn collectors_for_config(
        &self,
        config: &Config,
    ) -> anyhow::Result<Vec<Arc<dyn ProviderCollector>>> {
        if self.fixture_mode {
            Ok(Vec::new())
        } else {
            provider_registry::build_enabled(config, &self.storage).await
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
        match provider_id.as_str() {
            CODEX_PROVIDER_ID => self.add_codex_account(provider_id, display_name).await,
            CLAUDE_PROVIDER_ID => self.add_claude_account(provider_id, display_name).await,
            _ => anyhow::bail!("adding accounts is not supported for {provider_id}"),
        }
    }

    async fn add_codex_account(
        &self,
        provider_id: ProviderId,
        display_name: Option<String>,
    ) -> anyhow::Result<AddProviderAccountResponse> {
        let mutation = self.config_mutation.lock().await;
        let mut updated_config = self.config.read().await.clone();
        let provider = updated_config
            .providers
            .entry(CODEX_PROVIDER_ID.to_string())
            .or_insert_with(ProviderConfig::default);
        provider.enabled = true;
        if provider.profiles.is_empty() {
            provider.profiles.push(default_codex_profile()?);
        }
        let (profile_id, profile_path, profile_name) =
            if let Some(pending) = pending_codex_profile(provider) {
                pending
            } else {
                let profile_id = unique_profile_id(&provider.profiles, display_name.as_deref());
                let profile_path = codex_profile_home(&profile_id)?;
                std::fs::create_dir_all(&profile_path)?;
                provider.profiles.push(ProviderProfileConfig {
                    id: Some(profile_id.clone()),
                    display_name: display_name.clone(),
                    codex_home: Some(profile_path.clone()),
                    ..ProviderProfileConfig::default()
                });
                (profile_id, profile_path, display_name)
            };

        let collectors = self.collectors_for_config(&updated_config).await?;
        updated_config.persist()?;
        self.publish_local_log_config(&updated_config);
        *self.config.write().await = updated_config;
        self.refresh.set_providers(collectors).await;
        drop(mutation);

        let child = launchers::launch_codex_login(&profile_path)?;
        launchers::monitor_login(
            child,
            self.refresh.clone(),
            CODEX_PROVIDER_ID,
            Some(profile_id.clone()),
        );

        info!(
            provider_id = provider_id.as_str(),
            profile_id = profile_id.as_str(),
            profile_path = %profile_path.display(),
            "provider account login launched"
        );

        Ok(AddProviderAccountResponse {
            provider_id,
            profile_id,
            display_name: profile_name,
            profile_path: profile_path.display().to_string(),
        })
    }

    async fn add_claude_account(
        &self,
        provider_id: ProviderId,
        display_name: Option<String>,
    ) -> anyhow::Result<AddProviderAccountResponse> {
        let connected_profiles = self
            .storage
            .accounts()
            .await?
            .into_iter()
            .filter(|account| account.provider_id.as_str() == CLAUDE_PROVIDER_ID)
            .filter_map(|account| account.profile_id)
            .collect::<BTreeSet<_>>();

        let mutation = self.config_mutation.lock().await;
        let mut updated_config = self.config.read().await.clone();
        let provider = updated_config
            .providers
            .entry(CLAUDE_PROVIDER_ID.to_string())
            .or_default();
        provider.enabled = true;
        let target = if let Some(target) = pending_claude_profile(provider, &connected_profiles) {
            target
        } else {
            create_managed_claude_profile(provider, display_name.clone())?
        };

        let collectors = self.collectors_for_config(&updated_config).await?;
        updated_config.persist()?;
        self.publish_local_log_config(&updated_config);
        *self.config.write().await = updated_config;
        self.refresh.set_providers(collectors).await;
        drop(mutation);

        let profile_path = target
            .config_dir
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed Claude profile is missing its config path"))?;
        let child = launchers::launch_claude_login(Some(&profile_path))?;
        launchers::monitor_login(
            child,
            self.refresh.clone(),
            CLAUDE_PROVIDER_ID,
            Some(target.profile_id.clone()),
        );

        info!(
            provider_id = provider_id.as_str(),
            profile_id = target.profile_id.as_str(),
            profile_path = %profile_path.display(),
            "provider account login launched"
        );

        Ok(AddProviderAccountResponse {
            provider_id,
            profile_id: target.profile_id,
            display_name: target.display_name,
            profile_path: profile_path.display().to_string(),
        })
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
        if account.provider_id.as_str() == OPENCODE_GO_PROVIDER_ID {
            clear_cached_cookie_cache().await?;
        }

        let mutation = self.config_mutation.lock().await;
        let previous_config = self.config.read().await.clone();
        let plan = crate::runtime::account_service::plan_deletion(&previous_config, &account)?;
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
        ensure_supported_provider(&provider_id)?;
        let config = self.config.read().await.clone();
        let provider = config
            .providers
            .get(provider_id.as_str())
            .cloned()
            .unwrap_or_default();
        let profiles = provider
            .profiles
            .iter()
            .filter(|profile| !profile.deleted)
            .enumerate()
            .map(|(index, profile)| ProviderProfileResponse {
                id: profile
                    .id
                    .clone()
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or_else(|| {
                        if index == 0 {
                            "default".to_string()
                        } else {
                            format!("profile-{}", index + 1)
                        }
                    }),
                display_name: profile.display_name.clone(),
                enabled: profile.enabled,
            })
            .collect();

        let (mut workspace_options, discovery_error) =
            if provider_id.as_str() == OPENCODE_GO_PROVIDER_ID {
                let collector =
                    OpenCodeCollector::new(provider.clone(), config.debug_capture_raw_payloads)?;
                match collector.discover_workspace_options().await {
                    Ok(options) => (options, None),
                    Err(err) => (Vec::new(), Some(err.short_message().to_string())),
                }
            } else {
                (Vec::new(), None)
            };
        if let Some(selected) = provider.workspace_id.as_deref() {
            if !workspace_options.iter().any(|option| option == selected) {
                workspace_options.insert(0, selected.to_string());
            }
        }

        Ok(ProviderSetupResponse {
            provider_id,
            profiles,
            selected_workspace_id: provider.workspace_id,
            workspace_options,
            discovery_error,
        })
    }

    pub async fn update_provider_setup(
        &self,
        provider_id: ProviderId,
        workspace_id: Option<String>,
    ) -> anyhow::Result<ProviderSetupResponse> {
        ensure_supported_provider(&provider_id)?;
        if provider_id.as_str() != OPENCODE_GO_PROVIDER_ID {
            anyhow::bail!("workspace selection is only supported for OpenCode Go");
        }
        let workspace_id = workspace_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if workspace_id
            .as_deref()
            .is_some_and(|value| !value.starts_with("wrk_"))
        {
            anyhow::bail!("OpenCode workspace id must start with wrk_");
        }

        let mutation = self.config_mutation.lock().await;
        let mut updated_config = self.config.read().await.clone();
        updated_config
            .providers
            .entry(OPENCODE_GO_PROVIDER_ID.to_string())
            .or_default()
            .workspace_id = workspace_id;
        let collectors = self.collectors_for_config(&updated_config).await?;
        updated_config.persist()?;
        self.publish_local_log_config(&updated_config);
        *self.config.write().await = updated_config;
        self.refresh.set_providers(collectors).await;
        drop(mutation);
        self.provider_setup(provider_id).await
    }

    pub async fn repair_provider(
        &self,
        provider_id: ProviderId,
        account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
        if self.fixture_mode {
            anyhow::bail!("provider repair is unavailable in development fixture mode");
        }
        ensure_supported_provider(&provider_id)?;
        let message = match provider_id.as_str() {
            CODEX_PROVIDER_ID => {
                let config = self.config.read().await;
                let account = match account_id.as_ref() {
                    Some(id) => self.storage.account(id).await?,
                    None => None,
                };
                let requested_profile = account
                    .as_ref()
                    .and_then(|account| account.profile_id.as_deref());
                let profile = config
                    .providers
                    .get(CODEX_PROVIDER_ID)
                    .and_then(|provider| {
                        requested_profile
                            .and_then(|id| {
                                provider
                                    .profiles
                                    .iter()
                                    .find(|profile| profile.id.as_deref() == Some(id))
                            })
                            .or_else(|| provider.profiles.first())
                    });
                let home = profile
                    .and_then(|profile| profile.codex_home.clone())
                    .map(expand_home_path)
                    .unwrap_or(default_codex_profile()?.codex_home.unwrap_or_default());
                let profile_id = profile
                    .and_then(|profile| profile.id.clone())
                    .unwrap_or_else(|| "default".to_string());
                let child = launchers::launch_codex_login(&home)?;
                launchers::monitor_login(
                    child,
                    self.refresh.clone(),
                    CODEX_PROVIDER_ID,
                    Some(profile_id),
                );
                "Finish signing in to Codex in your browser. UsageTracker will refresh automatically."
                    .to_string()
            }
            CLAUDE_PROVIDER_ID => {
                let target = self
                    .prepare_claude_login_profile(account_id.as_ref())
                    .await?;
                let child = launchers::launch_claude_login(target.config_dir.as_deref())?;
                launchers::monitor_login(
                    child,
                    self.refresh.clone(),
                    CLAUDE_PROVIDER_ID,
                    Some(target.profile_id),
                );
                "Finish signing in to Claude in your browser. UsageTracker will refresh automatically."
                    .to_string()
            }
            OPENCODE_GO_PROVIDER_ID => {
                clear_cached_cookie_cache().await?;
                launchers::open_url("https://opencode.ai")?;
                "OpenCode opened in your browser. Sign in, then discover workspaces and refresh."
                    .to_string()
            }
            _ => unreachable!("supported provider validation should reject unknown ids"),
        };
        Ok(ProviderActionResponse {
            provider_id,
            message,
        })
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
        if account.provider_id.as_str() != CLAUDE_PROVIDER_ID {
            anyhow::bail!("profile sessions are currently supported for Claude only");
        }
        if !account.collection_enabled {
            anyhow::bail!("enable Claude account tracking before opening a profile session");
        }

        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = self.config.read().await;
        let provider = config
            .providers
            .get(CLAUDE_PROVIDER_ID)
            .ok_or_else(|| anyhow::anyhow!("Claude is not configured"))?;
        let config_dir = if provider.profiles.is_empty() && profile_id == "default" {
            None
        } else {
            let profile = provider
                .profiles
                .iter()
                .find(|profile| {
                    profile.enabled && !profile.deleted && profile.id.as_deref() == Some(profile_id)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Claude profile {profile_id} is no longer configured")
                })?;
            profile.claude_config_dir.clone().map(expand_home_path)
        };
        drop(config);

        let launcher =
            launchers::write_claude_profile_launcher(&account_id, config_dir.as_deref())?;
        launchers::open_terminal(&launcher)?;
        Ok(ProviderActionResponse {
            provider_id: account.provider_id,
            message: format!(
                "Opened a Claude session for {}. Activity from this terminal stays with this profile.",
                account
                    .display_name
                    .as_deref()
                    .unwrap_or(profile_id)
            ),
        })
    }

    async fn prepare_claude_login_profile(
        &self,
        account_id: Option<&AccountId>,
    ) -> anyhow::Result<ClaudeLoginTarget> {
        let requested_profile_id = match account_id {
            Some(account_id) => self
                .storage
                .account(account_id)
                .await?
                .and_then(|account| account.profile_id),
            None => None,
        };

        let mutation = self.config_mutation.lock().await;
        let mut updated_config = self.config.read().await.clone();
        let provider = updated_config
            .providers
            .entry(CLAUDE_PROVIDER_ID.to_string())
            .or_default();
        provider.enabled = true;
        let target = ensure_claude_login_profile(provider, requested_profile_id.as_deref())?;

        let collectors = self.collectors_for_config(&updated_config).await?;
        updated_config.persist()?;
        self.publish_local_log_config(&updated_config);
        *self.config.write().await = updated_config;
        self.refresh.set_providers(collectors).await;
        drop(mutation);
        Ok(target)
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

fn ensure_supported_provider(provider_id: &ProviderId) -> anyhow::Result<()> {
    provider_registry::ensure_supported(provider_id)
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
        if !schedule.visible.is_empty() {
            let report = refresh.refresh(Some(&schedule.visible)).await;
            info!(
                results = report.provider_results.len(),
                "initial visible refresh completed"
            );
        }
        let mut visible_due =
            tokio::time::Instant::now() + poll_delay(schedule.visible_interval_seconds);
        let mut hidden_due = tokio::time::Instant::now() + poll_delay(HIDDEN_PROVIDER_POLL_SECONDS);

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(visible_due), if !schedule.visible.is_empty() => {
                    let report = refresh.refresh(Some(&schedule.visible)).await;
                    info!(results = report.provider_results.len(), "visible provider poll completed");
                    visible_due = tokio::time::Instant::now()
                        + poll_delay(schedule.visible_interval_seconds);
                }
                _ = tokio::time::sleep_until(hidden_due), if !schedule.hidden.is_empty() => {
                    let report = refresh.refresh(Some(&schedule.hidden)).await;
                    info!(results = report.provider_results.len(), "hidden provider poll completed");
                    hidden_due = tokio::time::Instant::now()
                        + poll_delay(HIDDEN_PROVIDER_POLL_SECONDS);
                }
                changed = schedule_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    schedule = schedule_rx.borrow_and_update().clone();
                    visible_due = tokio::time::Instant::now()
                        + poll_delay(schedule.visible_interval_seconds);
                    hidden_due = tokio::time::Instant::now()
                        + poll_delay(HIDDEN_PROVIDER_POLL_SECONDS);
                    info!("provider poll schedule changed");
                }
            }
        }
    })
}

fn prepare_socket_path(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }

    if socket_path.exists() {
        match StdUnixStream::connect(socket_path) {
            Ok(_) => {
                anyhow::bail!(
                    "daemon socket {} is already accepting connections; refusing to replace a live daemon socket",
                    socket_path.display()
                );
            }
            Err(err) => {
                info!(
                    socket = %socket_path.display(),
                    error = %err,
                    "removing stale daemon socket"
                );
            }
        }
    }

    match std::fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::claude::keychain_service_for_config_dir;
    use crate::runtime::profile_service::push_managed_claude_profile;
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

        async fn discover_accounts(
            &self,
        ) -> Result<Vec<crate::providers::DiscoveredAccount>, crate::providers::ProviderError>
        {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
                self.first_finished.notify_one();
            }
            Ok(Vec::new())
        }

        async fn collect_usage(
            &self,
            _account: &crate::providers::DiscoveredAccount,
        ) -> Result<crate::providers::ProviderCollectionResult, crate::providers::ProviderError>
        {
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
            debug_capture_raw_payloads: false,
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
        assert_eq!(
            interval_rx.borrow_and_update().visible_interval_seconds,
            301
        );

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
            visible: vec![ProviderId::new(CODEX_PROVIDER_ID)],
            hidden: Vec::new(),
            visible_interval_seconds: 30,
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

        assert_eq!(schedule.visible, vec![ProviderId::new(CODEX_PROVIDER_ID)]);
        assert!(schedule
            .hidden
            .contains(&ProviderId::new(CLAUDE_PROVIDER_ID)));
        assert!(schedule
            .hidden
            .contains(&ProviderId::new(OPENCODE_GO_PROVIDER_ID)));
        assert_eq!(
            schedule.visible_interval_seconds,
            config.poll_interval_seconds
        );
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
    }

    #[test]
    fn pending_login_is_reused_and_profile_deletion_is_path_safe() {
        let managed_path = default_app_dir()
            .unwrap()
            .join("profiles")
            .join(CODEX_PROVIDER_ID)
            .join("pending-test");
        let provider = ProviderConfig {
            enabled: true,
            profiles: vec![ProviderProfileConfig {
                id: Some("pending-test".to_string()),
                display_name: Some("Pending".to_string()),
                codex_home: Some(managed_path.clone()),
                ..ProviderProfileConfig::default()
            }],
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
        assert!(profile.enabled);
        assert!(!profile.deleted);
        assert_eq!(profile.display_name.as_deref(), Some("Work"));
        assert_eq!(profile.claude_config_dir.as_deref(), Some(root.as_path()));
        assert_eq!(
            profile.keychain_service.as_deref(),
            Some(keychain_service_for_config_dir(&root).as_str())
        );
        assert_eq!(
            profile.credentials_file.as_deref(),
            Some(root.join(".credentials.json").as_path())
        );
        assert_eq!(profile.project_roots, vec![root.join("projects")]);
        assert!(profile.owns_default_claude_activity);
        assert_eq!(profile.cli_enabled, Some(true));
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
}
