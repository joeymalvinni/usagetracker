use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    os::unix::{fs::OpenOptionsExt, net::UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use tokio::sync::{watch, RwLock};
use tracing::{info, warn};
use usage_core::{
    default_app_dir, Account, AccountId, AddProviderAccountResponse, ConfigResponse,
    NotificationConfig, ProviderActionResponse, ProviderId, ProviderProfileResponse,
    ProviderSetupResponse, ProviderToggle,
};

use crate::{
    config::{Config, ProviderConfig, ProviderProfileConfig},
    health, local_logs,
    notifications::NotificationManager,
    polling::RefreshCoordinator,
    providers::{
        claude::{
            keychain_service_for_config_dir, ClaudeCollector, PROVIDER_ID as CLAUDE_PROVIDER_ID,
        },
        codex::{CodexCollector, PROVIDER_ID as CODEX_PROVIDER_ID},
        opencode::{clear_cached_cookie_cache, OpenCodeCollector, OPENCODE_GO_PROVIDER_ID},
        ProviderCollector,
    },
    server::SocketServer,
    storage::Storage,
};

pub struct Daemon {
    runtime: Arc<DaemonRuntime>,
    poll_interval_rx: watch::Receiver<u64>,
}

pub struct DaemonRuntime {
    config: RwLock<Config>,
    pub storage: Storage,
    pub refresh: Arc<RefreshCoordinator>,
    notifications: Arc<NotificationManager>,
    poll_interval_tx: watch::Sender<u64>,
}

struct ProviderRegistration {
    id: &'static str,
    build: fn(&Config) -> anyhow::Result<Arc<dyn ProviderCollector>>,
}

struct ClaudeLoginTarget {
    profile_id: String,
    display_name: Option<String>,
    config_dir: Option<PathBuf>,
}

const PROVIDER_REGISTRY: &[ProviderRegistration] = &[
    ProviderRegistration {
        id: CODEX_PROVIDER_ID,
        build: build_codex_provider,
    },
    ProviderRegistration {
        id: CLAUDE_PROVIDER_ID,
        build: build_claude_provider,
    },
    ProviderRegistration {
        id: OPENCODE_GO_PROVIDER_ID,
        build: build_opencode_go_provider,
    },
];

impl Daemon {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let storage = Storage::open(&config.paths.db)?;
        let providers = build_providers(&config, &storage).await?;
        let notifications = NotificationManager::new(storage.clone(), config.notifications.enabled);
        let refresh = Arc::new(RefreshCoordinator::with_notifications(
            storage.clone(),
            providers,
            notifications.clone(),
        ));
        let (runtime, poll_interval_rx) = DaemonRuntime::new(config, storage, refresh);

        Ok(Self {
            runtime,
            poll_interval_rx,
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

        let initial_refresh = self.runtime.refresh.clone();
        let initial_refresh_task = tokio::spawn(async move {
            let report = initial_refresh.refresh(None).await;
            info!(
                results = report.provider_results.len(),
                elapsed_ms = (report.finished_at - report.started_at).num_milliseconds(),
                "initial refresh completed"
            );
        });

        let mut poll_task = spawn_polling_loop(self.poll_interval_rx, self.runtime.refresh.clone());
        let claude_config = self
            .runtime
            .config
            .read()
            .await
            .providers
            .get(CLAUDE_PROVIDER_ID)
            .cloned()
            .unwrap_or_default();
        let local_log_task =
            local_logs::spawn_change_log_loop(self.runtime.refresh.clone(), claude_config);

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
        local_log_task.abort();
        initial_refresh_task.abort();
        let _ = server_task.await;
        let _ = poll_task.await;
        let _ = local_log_task.await;
        let _ = initial_refresh_task.await;
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
    ) -> (Arc<Self>, watch::Receiver<u64>) {
        let notifications = refresh.notification_manager();
        notifications.set_enabled(config.notifications.enabled);
        let (poll_interval_tx, poll_interval_rx) = watch::channel(config.poll_interval_seconds);
        let runtime = Arc::new(Self {
            config: RwLock::new(config),
            storage,
            refresh,
            notifications,
            poll_interval_tx,
        });
        (runtime, poll_interval_rx)
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
            self.refresh
                .provider_ids()
                .await
                .into_iter()
                .map(|id| id.as_str().to_string()),
        );
        Ok(providers)
    }

    pub async fn update_config(
        &self,
        poll_interval_seconds: Option<u64>,
        providers: Option<BTreeMap<String, ProviderToggle>>,
        notifications: Option<NotificationConfig>,
    ) -> anyhow::Result<ConfigResponse> {
        if let Some(providers) = &providers {
            for id in providers.keys() {
                if !PROVIDER_REGISTRY.iter().any(|r| r.id == id) {
                    anyhow::bail!("unknown provider: {id}");
                }
            }
        }

        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
        updated_config.apply_update(poll_interval_seconds, providers.as_ref(), notifications)?;

        let collectors = build_providers(&updated_config, &self.storage).await?;
        let notifications_reenabled =
            !config.notifications.enabled && updated_config.notifications.enabled;
        let notifications_disabled =
            config.notifications.enabled && !updated_config.notifications.enabled;
        if notifications_reenabled {
            self.storage.clear_notification_window_state().await?;
        }
        if notifications_disabled {
            self.storage.clear_pending_notifications().await?;
        }
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        self.notifications.set_enabled(config.notifications.enabled);
        let _ = self.poll_interval_tx.send(config.poll_interval_seconds);

        let poll_interval_seconds = config.poll_interval_seconds;
        let enabled_providers = config.enabled_provider_ids();
        info!(
            poll_interval_seconds,
            enabled_providers = ?enabled_providers,
            "daemon config updated"
        );
        drop(config);
        self.config_response().await
    }

    pub async fn add_provider_account(
        &self,
        provider_id: ProviderId,
        display_name: Option<String>,
    ) -> anyhow::Result<AddProviderAccountResponse> {
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
        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
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

        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        let _ = self.poll_interval_tx.send(config.poll_interval_seconds);
        drop(config);

        let child = launch_codex_login(&profile_path)?;
        monitor_provider_login(
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

        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
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

        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        let _ = self.poll_interval_tx.send(config.poll_interval_seconds);
        drop(config);

        let profile_path = target
            .config_dir
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed Claude profile is missing its config path"))?;
        let child = launch_claude_login(Some(&profile_path))?;
        monitor_provider_login(
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
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        self.apply_account_profile_updates(&account, display_name.as_deref(), collection_enabled)
            .await?;
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
        let managed_profile_path = self.mark_account_connection_deleted(&account).await?;
        self.storage.delete_account(&account_id).await?;
        if let Some(path) = managed_profile_path {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    warn!(path = %path.display(), error = %err, "failed to remove deleted account profile");
                }
            }
        }
        Ok(account_id)
    }

    async fn mark_account_connection_deleted(
        &self,
        account: &Account,
    ) -> anyhow::Result<Option<PathBuf>> {
        if account.provider_id.as_str() == OPENCODE_GO_PROVIDER_ID {
            clear_cached_cookie_cache().await?;
        }
        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
        let provider = updated_config
            .providers
            .entry(account.provider_id.as_str().to_string())
            .or_default();
        let mut managed_profile_path = None;

        if account.provider_id.as_str() == OPENCODE_GO_PROVIDER_ID {
            provider.enabled = false;
            provider.workspace_id = None;
            provider.cookie_header = None;
        } else if let Some(profile_id) = account.profile_id.as_deref() {
            if let Some(profile) = provider
                .profiles
                .iter_mut()
                .find(|profile| profile.id.as_deref() == Some(profile_id))
            {
                if account.provider_id.as_str() == CODEX_PROVIDER_ID {
                    managed_profile_path = profile
                        .codex_home
                        .as_ref()
                        .map(|path| expand_home_path(path.clone()))
                        .filter(|path| is_managed_codex_profile(path));
                } else if account.provider_id.as_str() == CLAUDE_PROVIDER_ID {
                    managed_profile_path = profile
                        .claude_config_dir
                        .as_ref()
                        .map(|path| expand_home_path(path.clone()))
                        .filter(|path| is_managed_claude_profile(path));
                }
                profile.enabled = false;
                profile.deleted = true;
                profile.display_name = None;
                profile.auth_path = None;
                profile.codex_home = None;
                profile.keychain_account = None;
                profile.keychain_service = None;
                profile.credentials_file = None;
                profile.claude_config_dir = None;
                profile.cli_enabled = None;
                profile.project_roots.clear();
                profile.owns_default_codex_activity = false;
                profile.owns_default_claude_activity = false;
            } else {
                provider.profiles.push(ProviderProfileConfig {
                    id: Some(profile_id.to_string()),
                    enabled: false,
                    deleted: true,
                    ..ProviderProfileConfig::default()
                });
            }
            if !provider
                .profiles
                .iter()
                .any(|profile| profile.enabled && !profile.deleted)
            {
                provider.enabled = false;
            }
        }

        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        let _ = self.poll_interval_tx.send(config.poll_interval_seconds);
        Ok(managed_profile_path)
    }

    async fn apply_account_profile_updates(
        &self,
        account: &Account,
        display_name: Option<&str>,
        collection_enabled: Option<bool>,
    ) -> anyhow::Result<()> {
        let display_name = display_name
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if display_name.is_none() && collection_enabled.is_none() {
            return Ok(());
        }
        let Some(profile_id) = account.profile_id.as_deref() else {
            return Ok(());
        };

        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
        let Some(provider) = updated_config
            .providers
            .get_mut(account.provider_id.as_str())
        else {
            return Ok(());
        };
        let Some(profile) = provider
            .profiles
            .iter_mut()
            .find(|profile| !profile.deleted && profile.id.as_deref() == Some(profile_id))
        else {
            return Ok(());
        };
        let mut changed = false;
        if let Some(display_name) = display_name {
            if profile.display_name.as_deref() != Some(display_name) {
                profile.display_name = Some(display_name.to_string());
                changed = true;
            }
        }
        if let Some(collection_enabled) = collection_enabled {
            if profile.enabled != collection_enabled {
                profile.enabled = collection_enabled;
                changed = true;
            }
            if collection_enabled && !provider.enabled {
                provider.enabled = true;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        let _ = self.poll_interval_tx.send(config.poll_interval_seconds);
        Ok(())
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

        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
        updated_config
            .providers
            .entry(OPENCODE_GO_PROVIDER_ID.to_string())
            .or_default()
            .workspace_id = workspace_id;
        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        drop(config);
        self.provider_setup(provider_id).await
    }

    pub async fn repair_provider(
        &self,
        provider_id: ProviderId,
        account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
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
                let child = launch_codex_login(&home)?;
                monitor_provider_login(
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
                let child = launch_claude_login(target.config_dir.as_deref())?;
                monitor_provider_login(
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
                open_url("https://opencode.ai")?;
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

        let launcher = write_claude_profile_launcher(&account_id, config_dir.as_deref())?;
        open_terminal_launcher(&launcher)?;
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

        let mut config = self.config.write().await;
        let mut updated_config = config.clone();
        let provider = updated_config
            .providers
            .entry(CLAUDE_PROVIDER_ID.to_string())
            .or_default();
        provider.enabled = true;
        let target = ensure_claude_login_profile(provider, requested_profile_id.as_deref())?;

        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
        let _ = self.poll_interval_tx.send(config.poll_interval_seconds);
        Ok(target)
    }
}

fn ensure_supported_provider(provider_id: &ProviderId) -> anyhow::Result<()> {
    if PROVIDER_REGISTRY
        .iter()
        .any(|registration| registration.id == provider_id.as_str())
    {
        Ok(())
    } else {
        anyhow::bail!("unknown provider: {provider_id}")
    }
}

fn expand_home_path(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if value == "~" {
        return dirs::home_dir().unwrap_or(path);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path
}

fn codex_profile_home(profile_id: &str) -> anyhow::Result<PathBuf> {
    let app_dir = default_app_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve ~/.usagetracker directory"))?;
    Ok(app_dir
        .join("profiles")
        .join(CODEX_PROVIDER_ID)
        .join(profile_id))
}

fn claude_profile_home(profile_id: &str) -> anyhow::Result<PathBuf> {
    let app_dir = default_app_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve ~/.usagetracker directory"))?;
    Ok(app_dir
        .join("profiles")
        .join(CLAUDE_PROVIDER_ID)
        .join(profile_id))
}

fn pending_codex_profile(provider: &ProviderConfig) -> Option<(String, PathBuf, Option<String>)> {
    provider.profiles.iter().rev().find_map(|profile| {
        if !profile.enabled || profile.deleted {
            return None;
        }
        let profile_id = profile.id.clone()?;
        let profile_path = expand_home_path(profile.codex_home.clone()?);
        if !is_managed_codex_profile(&profile_path) {
            return None;
        }
        let auth_path = profile
            .auth_path
            .clone()
            .map(expand_home_path)
            .unwrap_or_else(|| profile_path.join("auth.json"));
        (!auth_path.exists()).then(|| (profile_id, profile_path, profile.display_name.clone()))
    })
}

fn is_managed_codex_profile(path: &Path) -> bool {
    default_app_dir()
        .map(|root| path.starts_with(root.join("profiles").join(CODEX_PROVIDER_ID)))
        .unwrap_or(false)
}

fn is_managed_claude_profile(path: &Path) -> bool {
    default_app_dir()
        .map(|root| path.starts_with(root.join("profiles").join(CLAUDE_PROVIDER_ID)))
        .unwrap_or(false)
}

fn default_codex_profile() -> anyhow::Result<ProviderProfileConfig> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("failed to resolve home directory"))?;
    Ok(ProviderProfileConfig {
        id: Some("default".to_string()),
        display_name: None,
        codex_home: Some(home.join(".codex")),
        ..ProviderProfileConfig::default()
    })
}

fn ensure_claude_login_profile(
    provider: &mut ProviderConfig,
    requested_profile_id: Option<&str>,
) -> anyhow::Result<ClaudeLoginTarget> {
    if provider.profiles.is_empty() && requested_profile_id == Some("default") {
        return Ok(ClaudeLoginTarget {
            profile_id: "default".to_string(),
            display_name: None,
            config_dir: None,
        });
    }
    if let Some(requested_profile_id) = requested_profile_id {
        if let Some((index, profile)) =
            provider
                .profiles
                .iter_mut()
                .enumerate()
                .find(|(_, profile)| {
                    !profile.deleted && profile.id.as_deref() == Some(requested_profile_id)
                })
        {
            profile.enabled = true;
            return Ok(claude_login_target(profile, index));
        }
        anyhow::bail!("Claude profile {requested_profile_id} is no longer configured");
    }

    if let Some((index, profile)) = provider
        .profiles
        .iter()
        .enumerate()
        .find(|(_, profile)| profile.enabled && !profile.deleted)
    {
        return Ok(claude_login_target(profile, index));
    }

    create_managed_claude_profile(provider, None)
}

fn create_managed_claude_profile(
    provider: &mut ProviderConfig,
    display_name: Option<String>,
) -> anyhow::Result<ClaudeLoginTarget> {
    let profile_id = unique_profile_id(&provider.profiles, display_name.as_deref());
    let config_dir = claude_profile_home(&profile_id)?;
    push_managed_claude_profile(provider, profile_id, display_name, config_dir)
}

fn push_managed_claude_profile(
    provider: &mut ProviderConfig,
    profile_id: String,
    display_name: Option<String>,
    config_dir: PathBuf,
) -> anyhow::Result<ClaudeLoginTarget> {
    let keychain_account = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    let owns_default_claude_activity = !provider
        .profiles
        .iter()
        .any(|profile| profile.enabled && !profile.deleted);
    std::fs::create_dir_all(&config_dir)?;
    let keychain_service = keychain_service_for_config_dir(&config_dir);
    provider.profiles.push(ProviderProfileConfig {
        id: Some(profile_id.clone()),
        display_name: display_name.clone(),
        keychain_account: Some(keychain_account),
        keychain_service: Some(keychain_service),
        credentials_file: Some(config_dir.join(".credentials.json")),
        claude_config_dir: Some(config_dir.clone()),
        cli_enabled: Some(true),
        project_roots: vec![config_dir.join("projects")],
        owns_default_claude_activity,
        ..ProviderProfileConfig::default()
    });
    Ok(ClaudeLoginTarget {
        profile_id,
        display_name,
        config_dir: Some(config_dir),
    })
}

fn pending_claude_profile(
    provider: &ProviderConfig,
    connected_profiles: &BTreeSet<String>,
) -> Option<ClaudeLoginTarget> {
    provider
        .profiles
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, profile)| {
            if !profile.enabled || profile.deleted {
                return None;
            }
            let target = claude_login_target(profile, index);
            let config_dir = target.config_dir.as_deref()?;
            (is_managed_claude_profile(config_dir)
                && !connected_profiles.contains(&target.profile_id))
            .then_some(target)
        })
}

fn claude_login_target(profile: &ProviderProfileConfig, index: usize) -> ClaudeLoginTarget {
    let profile_id = profile
        .id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| {
            if index == 0 {
                "default".to_string()
            } else {
                format!("profile-{}", index + 1)
            }
        });
    ClaudeLoginTarget {
        profile_id,
        display_name: profile.display_name.clone(),
        config_dir: profile.claude_config_dir.clone().map(expand_home_path),
    }
}

fn unique_profile_id(profiles: &[ProviderProfileConfig], display_name: Option<&str>) -> String {
    let base = display_name
        .map(slugify_profile_id)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "account".to_string());
    let existing = profiles
        .iter()
        .filter_map(|profile| profile.id.as_deref())
        .collect::<BTreeSet<_>>();
    if !existing.contains(base.as_str()) {
        return base;
    }
    for index in 2.. {
        let candidate = format!("{base}-{index}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("infinite profile id search should always return")
}

fn slugify_profile_id(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn launch_codex_login(codex_home: &Path) -> anyhow::Result<std::process::Child> {
    let mut direct = Command::new("codex");
    configure_codex_login_command(&mut direct, codex_home);
    match direct.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Apps launched from Finder commonly receive a minimal PATH. Resolve the user's
            // Codex installation through their interactive login shell without opening Terminal.
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
            let mut fallback = Command::new(shell);
            fallback.args(["-lic", "exec codex login"]);
            configure_codex_login_stdio(&mut fallback, codex_home);
            fallback.spawn().map_err(|fallback_err| {
                anyhow::anyhow!("failed to start Codex login: {fallback_err}")
            })
        }
        Err(err) => Err(anyhow::anyhow!("failed to start Codex login: {err}")),
    }
}

fn configure_codex_login_command(command: &mut Command, codex_home: &Path) {
    command.arg("login");
    configure_codex_login_stdio(command, codex_home);
}

fn configure_codex_login_stdio(command: &mut Command, codex_home: &Path) {
    command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        // Keep diagnostics in usage-daemon.log while Codex opens the browser itself.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
}

fn monitor_provider_login(
    mut child: std::process::Child,
    refresh: Arc<RefreshCoordinator>,
    provider_id: &'static str,
    profile_id: Option<String>,
) {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {
            info!(
                provider_id,
                profile_id, "provider login completed; refreshing account"
            );
            runtime.spawn(async move {
                let provider = ProviderId::new(provider_id);
                let report = refresh.refresh(Some(std::slice::from_ref(&provider))).await;
                info!(
                    provider_id,
                    profile_id,
                    results = report.provider_results.len(),
                    "post-login provider refresh completed"
                );
            });
        }
        Ok(status) => {
            warn!(provider_id, profile_id, %status, "provider login process exited unsuccessfully");
        }
        Err(err) => {
            warn!(provider_id, profile_id, error = %err, "failed to wait for provider login process");
        }
    });
}

fn launch_claude_login(config_dir: Option<&Path>) -> anyhow::Result<std::process::Child> {
    let mut direct = Command::new("claude");
    direct.args(["auth", "login"]);
    configure_claude_login_environment(&mut direct, config_dir);
    configure_browser_login_stdio(&mut direct);
    match direct.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Finder-launched apps usually do not inherit ~/.local/bin or other shell PATH setup.
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
            let mut fallback = Command::new(shell);
            fallback.args(["-lic", "exec claude auth login"]);
            configure_claude_login_environment(&mut fallback, config_dir);
            configure_browser_login_stdio(&mut fallback);
            fallback.spawn().map_err(|fallback_err| {
                anyhow::anyhow!("failed to start Claude login: {fallback_err}")
            })
        }
        Err(err) => Err(anyhow::anyhow!("failed to start Claude login: {err}")),
    }
}

fn configure_claude_login_environment(command: &mut Command, config_dir: Option<&Path>) {
    if let Some(config_dir) = config_dir {
        command
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
}

fn configure_browser_login_stdio(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
}

fn write_claude_profile_launcher(
    account_id: &AccountId,
    config_dir: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let app_dir = default_app_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve ~/.usagetracker directory"))?;
    let launcher_dir = app_dir.join("launchers");
    std::fs::create_dir_all(&launcher_dir)?;
    let launcher = launcher_dir.join(format!("claude-{}.command", account_id.as_str()));
    let temporary = launcher_dir.join(format!(
        ".claude-{}.{}.tmp",
        account_id.as_str(),
        uuid::Uuid::new_v4()
    ));
    let contents = claude_launcher_contents(config_dir);
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &launcher)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(launcher)
}

fn claude_launcher_contents(config_dir: Option<&Path>) -> String {
    let profile_setup = match config_dir {
        Some(path) => format!(
            "export CLAUDE_CONFIG_DIR={}\n",
            shell_single_quote(&path.display().to_string())
        ),
        None => "unset CLAUDE_CONFIG_DIR\n".to_string(),
    };
    format!("#!/bin/zsh -l\nunset CLAUDE_SECURESTORAGE_CONFIG_DIR\n{profile_setup}exec claude\n")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn open_terminal_launcher(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open")
            .args(["-a", "Terminal"])
            .arg(path)
            .status()
            .map_err(|err| anyhow::anyhow!("failed to open Claude profile terminal: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("failed to open Claude profile terminal")
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        anyhow::bail!("opening Claude profile terminals is only supported on macOS")
    }
}

fn open_url(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open")
            .arg(url)
            .status()
            .map_err(|err| anyhow::anyhow!("failed to open {url}: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("failed to open {url}")
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        anyhow::bail!("opening provider login is only supported on macOS")
    }
}

async fn build_providers(
    config: &Config,
    storage: &Storage,
) -> anyhow::Result<Vec<Arc<dyn ProviderCollector>>> {
    let mut providers: Vec<Arc<dyn ProviderCollector>> = Vec::new();
    for registration in PROVIDER_REGISTRY {
        if config.provider_enabled(registration.id) {
            providers.push((registration.build)(config)?);
        } else {
            storage
                .upsert_health(&health::disabled(ProviderId::new(registration.id)))
                .await?;
        }
    }
    Ok(providers)
}

fn build_codex_provider(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(CodexCollector::new(
        config
            .providers
            .get(CODEX_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}

fn build_claude_provider(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(ClaudeCollector::new(
        config
            .providers
            .get(CLAUDE_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}

fn build_opencode_go_provider(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(OpenCodeCollector::new(
        config
            .providers
            .get(OPENCODE_GO_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}

fn spawn_polling_loop(
    mut interval_rx: watch::Receiver<u64>,
    refresh: Arc<RefreshCoordinator>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let seconds = *interval_rx.borrow_and_update();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(seconds)) => {
                    let report = refresh.refresh(None).await;
                    info!(
                        results = report.provider_results.len(),
                        "poll refresh completed"
                    );
                }
                changed = interval_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    info!(
                        poll_interval_seconds = *interval_rx.borrow(),
                        "poll interval changed"
                    );
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
        assert!(is_managed_codex_profile(&pending.1));
        assert!(!is_managed_codex_profile(
            &dirs::home_dir().unwrap().join(".codex")
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
        let contents = claude_launcher_contents(Some(Path::new("/tmp/Claude's Work")));

        assert!(contents.contains("unset CLAUDE_SECURESTORAGE_CONFIG_DIR"));
        assert!(contents.contains("export CLAUDE_CONFIG_DIR='/tmp/Claude'\"'\"'s Work'"));
        assert!(contents.ends_with("exec claude\n"));
    }

    #[test]
    fn legacy_claude_launcher_clears_profile_overrides() {
        let contents = claude_launcher_contents(None);

        assert!(contents.contains("unset CLAUDE_CONFIG_DIR"));
        assert!(!contents.contains("export CLAUDE_CONFIG_DIR"));
    }
}
