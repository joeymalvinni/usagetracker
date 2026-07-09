use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::net::UnixStream as StdUnixStream,
    path::Path,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{watch, RwLock};
use tracing::{info, warn};
use usage_core::{ConfigResponse, ProviderId, ProviderToggle};

use crate::{
    config::Config,
    health, local_logs,
    polling::RefreshCoordinator,
    providers::{
        claude::{ClaudeCollector, PROVIDER_ID as CLAUDE_PROVIDER_ID},
        codex::{CodexCollector, PROVIDER_ID as CODEX_PROVIDER_ID},
        opencode::{OpenCodeCollector, OPENCODE_GO_PROVIDER_ID},
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
    poll_interval_tx: watch::Sender<u64>,
}

struct ProviderRegistration {
    id: &'static str,
    build: fn(&Config) -> anyhow::Result<Arc<dyn ProviderCollector>>,
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
        let refresh = Arc::new(RefreshCoordinator::new(storage.clone(), providers));
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
        let server_task = {
            let socket_path = socket_path.clone();
            tokio::spawn(async move { server.run(&socket_path).await })
        };

        let initial_refresh = self.runtime.refresh.refresh(None).await;
        info!(
            results = initial_refresh.provider_results.len(),
            "initial refresh completed"
        );

        let poll_task = spawn_polling_loop(self.poll_interval_rx, self.runtime.refresh.clone());
        let local_log_task = local_logs::spawn_change_log_loop(self.runtime.refresh.clone());

        tokio::signal::ctrl_c().await?;
        info!("shutdown signal received");

        server_task.abort();
        poll_task.abort();
        local_log_task.abort();
        if let Err(err) = std::fs::remove_file(&socket_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(error = %err, "failed to remove socket file");
            }
        }
        Ok(())
    }
}

impl DaemonRuntime {
    pub fn new(
        config: Config,
        storage: Storage,
        refresh: Arc<RefreshCoordinator>,
    ) -> (Arc<Self>, watch::Receiver<u64>) {
        let (poll_interval_tx, poll_interval_rx) = watch::channel(config.poll_interval_seconds);
        let runtime = Arc::new(Self {
            config: RwLock::new(config),
            storage,
            refresh,
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
        updated_config.apply_update(poll_interval_seconds, providers.as_ref())?;

        let collectors = build_providers(&updated_config, &self.storage).await?;
        updated_config.persist()?;
        *config = updated_config;
        self.refresh.set_providers(collectors).await;
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
        config.debug_capture_raw_payloads,
    )?))
}

fn build_claude_provider(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(ClaudeCollector::new(
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
