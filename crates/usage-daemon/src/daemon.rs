use std::{path::Path, sync::Arc, time::Duration};

use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use usage_core::ProviderId;

use crate::{
    config::Config,
    health,
    polling::RefreshCoordinator,
    providers::{
        claude::{ClaudeCollector, PROVIDER_ID as CLAUDE_PROVIDER_ID},
        codex::{CodexCollector, PROVIDER_ID as CODEX_PROVIDER_ID},
        ProviderCollector,
    },
    server::SocketServer,
    storage::Storage,
};

pub struct Daemon {
    config: Arc<Config>,
    storage: Storage,
    refresh: Arc<RefreshCoordinator>,
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
];

impl Daemon {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let storage = Storage::open(&config.paths.db)?;
        let mut providers: Vec<Arc<dyn ProviderCollector>> = Vec::new();

        for registration in PROVIDER_REGISTRY {
            if config.provider_enabled(registration.id) {
                providers.push((registration.build)(&config)?);
            } else {
                storage
                    .upsert_health(&health::disabled(ProviderId::new(registration.id)))
                    .await?;
            }
        }

        let refresh = Arc::new(RefreshCoordinator::new(storage.clone(), providers));
        Ok(Self {
            config: Arc::new(config),
            storage,
            refresh,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        prepare_socket_path(&self.config.paths.socket)?;

        let server = SocketServer::new(
            self.config.clone(),
            self.storage.clone(),
            self.refresh.clone(),
        );
        let socket_path = self.config.paths.socket.clone();
        let server_task = tokio::spawn(async move { server.run(&socket_path).await });

        let initial_refresh = self.refresh.refresh(None).await;
        info!(
            results = initial_refresh.provider_results.len(),
            "initial refresh completed"
        );

        let poll_task = spawn_polling_loop(self.config.clone(), self.refresh.clone());

        tokio::signal::ctrl_c().await?;
        info!("shutdown signal received");

        server_task.abort();
        poll_task.abort();
        if let Err(err) = std::fs::remove_file(&self.config.paths.socket) {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(error = %err, "failed to remove socket file");
            }
        }
        Ok(())
    }
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

fn spawn_polling_loop(
    config: Arc<Config>,
    refresh: Arc<RefreshCoordinator>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(config.poll_interval_seconds));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;

        loop {
            interval.tick().await;
            let report = refresh.refresh(None).await;
            info!(
                results = report.provider_results.len(),
                "poll refresh completed"
            );
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
    match std::fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}
