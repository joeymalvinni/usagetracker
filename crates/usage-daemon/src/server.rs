use std::{path::Path, sync::Arc};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::{debug, info, warn};
use usage_core::{ApiRequest, ApiResponse};

use crate::{config::Config, polling::RefreshCoordinator, storage::Storage};

#[derive(Clone)]
pub struct SocketServer {
    config: Arc<Config>,
    storage: Storage,
    refresh: Arc<RefreshCoordinator>,
}

impl SocketServer {
    pub fn new(config: Arc<Config>, storage: Storage, refresh: Arc<RefreshCoordinator>) -> Self {
        Self {
            config,
            storage,
            refresh,
        }
    }

    pub async fn run(self, socket_path: &Path) -> anyhow::Result<()> {
        let listener = UnixListener::bind(socket_path)?;
        tracing::info!(socket = %socket_path.display(), "daemon socket listening");

        loop {
            let (stream, _) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(err) = server.handle_client(stream).await {
                    debug!(error = %err, "client connection ended");
                }
            });
        }
    }

    async fn handle_client(&self, stream: UnixStream) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let response = match serde_json::from_str::<ApiRequest>(&line) {
                Ok(request) => {
                    info!(request = ?request, "daemon request received");
                    let response = self.handle_request(request).await;
                    debug!(response = ?response, "daemon response completed");
                    response
                }
                Err(err) => {
                    warn!(error = %err, "invalid daemon request JSON");
                    ApiResponse::error("invalid_json", format!("invalid request JSON: {err}"))
                }
            };
            let bytes = serde_json::to_vec(&response)?;
            writer.write_all(&bytes).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }

        Ok(())
    }

    async fn handle_request(&self, request: ApiRequest) -> ApiResponse {
        match request {
            ApiRequest::GetUsage => match self.storage.latest_usage() {
                Ok(snapshots) => ApiResponse::Usage { snapshots },
                Err(err) => storage_error(err),
            },
            ApiRequest::Refresh { providers } => {
                let report = self.refresh.refresh(providers.as_deref()).await;
                ApiResponse::Refresh {
                    started_at: report.started_at,
                    finished_at: report.finished_at,
                    provider_results: report.provider_results,
                }
            }
            ApiRequest::GetProviderHealth => match self.storage.provider_health() {
                Ok(health) => ApiResponse::ProviderHealth { health },
                Err(err) => storage_error(err),
            },
            ApiRequest::GetAccounts => match self.storage.accounts() {
                Ok(accounts) => ApiResponse::Accounts { accounts },
                Err(err) => storage_error(err),
            },
            ApiRequest::GetConfig => ApiResponse::Config {
                config: self.config.response(),
            },
        }
    }
}

fn storage_error(err: anyhow::Error) -> ApiResponse {
    warn!(error = %err, "storage request failed");
    ApiResponse::error("storage_error", err.to_string())
}
