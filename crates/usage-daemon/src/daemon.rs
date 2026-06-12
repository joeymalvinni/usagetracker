use std::{path::Path, sync::Arc, time::Duration};

use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use usage_core::ProviderId;

use crate::{
    config::Config,
    health,
    polling::RefreshCoordinator,
    providers::{claude::ClaudeCollector, codex::CodexCollector, ProviderCollector},
    server::SocketServer,
    storage::Storage,
};

pub struct Daemon {
    config: Arc<Config>,
    storage: Storage,
    refresh: Arc<RefreshCoordinator>,
}

impl Daemon {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let storage = Storage::open(&config.paths.db)?;
        let mut providers: Vec<Arc<dyn ProviderCollector>> = Vec::new();

        if config.provider_enabled("codex") {
            providers.push(Arc::new(CodexCollector::new(
                config.debug_capture_raw_payloads,
            )?));
        } else {
            storage.upsert_health(&health::disabled(ProviderId::new("codex")))?;
        }

        if config.provider_enabled("claude") {
            providers.push(Arc::new(ClaudeCollector::new(
                config.debug_capture_raw_payloads,
            )?));
        } else {
            storage.upsert_health(&health::disabled(ProviderId::new("claude")))?;
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
