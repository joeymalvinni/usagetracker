use std::{path::Path, sync::Arc};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::{debug, info, warn};
use usage_core::{ApiRequest, ApiResponse};

use crate::daemon::DaemonRuntime;

#[derive(Clone)]
pub struct SocketServer {
    runtime: Arc<DaemonRuntime>,
}

impl SocketServer {
    pub fn new(runtime: Arc<DaemonRuntime>) -> Self {
        Self { runtime }
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
            ApiRequest::GetUsage => match self.runtime.storage.latest_usage().await {
                Ok(snapshots) => ApiResponse::Usage { snapshots },
                Err(err) => storage_error(err),
            },
            ApiRequest::Refresh { providers } => {
                let report = self.runtime.refresh.refresh(providers.as_deref()).await;
                ApiResponse::Refresh {
                    started_at: report.started_at,
                    finished_at: report.finished_at,
                    provider_results: report.provider_results,
                }
            }
            ApiRequest::GetProviderHealth => match self.runtime.storage.provider_health().await {
                Ok(health) => ApiResponse::ProviderHealth { health },
                Err(err) => storage_error(err),
            },
            ApiRequest::GetAccounts => match self.runtime.storage.accounts().await {
                Ok(accounts) => ApiResponse::Accounts { accounts },
                Err(err) => storage_error(err),
            },
            ApiRequest::GetConfig => ApiResponse::Config {
                config: self.runtime.config_response().await,
            },
            ApiRequest::UpdateConfig {
                poll_interval_seconds,
                providers,
            } => match self
                .runtime
                .update_config(poll_interval_seconds, providers)
                .await
            {
                Ok(config) => ApiResponse::Config { config },
                Err(err) => {
                    warn!(error = %err, "config update failed");
                    ApiResponse::error("invalid_config", err.to_string())
                }
            },
        }
    }
}

fn storage_error(err: anyhow::Error) -> ApiResponse {
    warn!(error = %err, "storage request failed");
    ApiResponse::error("storage_error", err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polling::RefreshCoordinator;
    use std::collections::BTreeMap;
    use tokio::time::{timeout, Duration};
    use usage_core::ProviderId;
    use uuid::Uuid;

    struct TestEnv {
        root: std::path::PathBuf,
        socket_path: std::path::PathBuf,
        runtime: Arc<DaemonRuntime>,
    }

    fn test_env(providers: BTreeMap<String, crate::config::ProviderConfig>) -> TestEnv {
        let root = std::env::temp_dir().join(format!("usage-server-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let socket_path = Path::new("/tmp").join(format!("usage-{}.sock", Uuid::new_v4()));
        let db_path = root.join("usage.sqlite3");
        let config_path = root.join("config.json");

        let storage = crate::storage::Storage::open(&db_path).unwrap();
        let config = crate::config::Config {
            poll_interval_seconds: 30,
            providers,
            debug_capture_raw_payloads: false,
            paths: crate::config::Paths {
                config: config_path,
                db: db_path,
                socket: socket_path.clone(),
            },
        };
        let refresh = Arc::new(RefreshCoordinator::new(storage.clone(), Vec::new()));
        let (runtime, _poll_rx) = DaemonRuntime::new(config, storage, refresh);
        TestEnv {
            root,
            socket_path,
            runtime,
        }
    }

    async fn request_line(socket_path: &Path, request: &str) -> ApiResponse {
        let mut stream = UnixStream::connect(socket_path).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        let mut lines = BufReader::new(stream).lines();
        let response = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&response).unwrap()
    }

    #[tokio::test]
    async fn serves_config_request_over_socket() {
        let env = test_env(BTreeMap::new());
        let server = SocketServer::new(env.runtime.clone());

        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let response = request_line(&env.socket_path, r#"{"method":"get_config"}"#).await;

        match response {
            ApiResponse::Config { config } => {
                assert_eq!(config.poll_interval_seconds, 30);
                assert_eq!(config.enabled_providers, Vec::<ProviderId>::new());
            }
            other => panic!("unexpected response: {other:?}"),
        }

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn updates_config_over_socket() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            crate::config::ProviderConfig { enabled: true },
        );
        let env = test_env(providers);
        let config_path = env.root.join("config.json");
        let server = SocketServer::new(env.runtime.clone());

        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let response = request_line(
            &env.socket_path,
            r#"{"method":"update_config","poll_interval_seconds":120,"providers":{"codex":{"enabled":false}}}"#,
        )
        .await;

        match response {
            ApiResponse::Config { config } => {
                assert_eq!(config.poll_interval_seconds, 120);
                assert_eq!(config.enabled_providers, Vec::<ProviderId>::new());
                assert!(!config.providers["codex"].enabled);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(persisted["poll_interval_seconds"], 120);
        assert_eq!(persisted["providers"]["codex"]["enabled"], false);

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn rejects_unknown_provider_in_config_update() {
        let env = test_env(BTreeMap::new());
        let server = SocketServer::new(env.runtime.clone());

        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let response = request_line(
            &env.socket_path,
            r#"{"method":"update_config","providers":{"nonsense":{"enabled":true}}}"#,
        )
        .await;

        match response {
            ApiResponse::Error { error } => assert_eq!(error.code, "invalid_config"),
            other => panic!("unexpected response: {other:?}"),
        }

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    async fn wait_for_socket(socket_path: &Path) {
        for _ in 0..20 {
            if socket_path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("socket was not created");
    }
}
