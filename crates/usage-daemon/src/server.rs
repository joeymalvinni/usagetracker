use std::{
    collections::{BTreeSet, HashMap},
    io,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};
use tracing::{debug, trace, warn};
use usage_core::{
    Account, ApiRequest, ApiResponse, ProviderHealth, UsageAmount, UsageSnapshot, UsageUnit,
    UsageWindow, UsageWindowKind,
};

use crate::{daemon::DaemonRuntime, forecast, storage::StoredDailyUsageHistory};

const MAX_CLIENT_CONNECTIONS: usize = 64;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DASHBOARD_HISTORY_DAYS: u64 = 30;
const FORECAST_HISTORY_DAYS: i64 = 35;
const FORECAST_HISTORY_LIMIT: usize = 10_000;

#[derive(Clone)]
pub struct SocketServer {
    runtime: Arc<DaemonRuntime>,
}

impl SocketServer {
    pub fn new(runtime: Arc<DaemonRuntime>) -> Self {
        Self { runtime }
    }

    #[cfg(test)]
    pub async fn run(self, socket_path: &Path) -> anyhow::Result<()> {
        let listener = Self::bind(socket_path)?;
        self.serve(listener, socket_path).await
    }

    pub fn bind(socket_path: &Path) -> io::Result<UnixListener> {
        let listener = UnixListener::bind(socket_path)?;
        let permission_result = (|| {
            let mut permissions = std::fs::metadata(socket_path)?.permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(socket_path, permissions)
        })();
        match permission_result {
            Ok(()) => Ok(listener),
            Err(err) => {
                drop(listener);
                let _ = std::fs::remove_file(socket_path);
                Err(err)
            }
        }
    }

    pub async fn serve(self, listener: UnixListener, socket_path: &Path) -> anyhow::Result<()> {
        tracing::info!(socket = %socket_path.display(), "daemon socket listening");
        let connections = Arc::new(Semaphore::new(MAX_CLIENT_CONNECTIONS));

        loop {
            let (stream, _) = listener.accept().await?;
            let Ok(permit) = connections.clone().try_acquire_owned() else {
                debug!(
                    max_connections = MAX_CLIENT_CONNECTIONS,
                    "rejecting daemon client because the connection limit was reached"
                );
                continue;
            };
            let server = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(err) = server.handle_client(stream).await {
                    debug!(error = %err, "client connection ended");
                }
            });
        }
    }

    async fn handle_client(&self, stream: UnixStream) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::with_capacity(8 * 1024, reader);
        let mut line = Vec::with_capacity(1024);

        loop {
            let frame = match tokio::time::timeout(
                CLIENT_IDLE_TIMEOUT,
                read_request_frame(&mut reader, &mut line),
            )
            .await
            {
                Ok(frame) => frame?,
                Err(_) => {
                    debug!("closing idle daemon client connection");
                    return Ok(());
                }
            };
            let RequestFrame::Line(line) = frame else {
                if frame == RequestFrame::TooLarge {
                    let response = ApiResponse::error(
                        "request_too_large",
                        format!("request exceeds the {MAX_REQUEST_BYTES}-byte limit"),
                    );
                    writer.write_all(&serde_json::to_vec(&response)?).await?;
                    writer.write_all(b"\n").await?;
                }
                return Ok(());
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            let response = match serde_json::from_slice::<ApiRequest>(line) {
                Ok(request) => {
                    debug!(request = ?request, "daemon request received");
                    let started = Instant::now();
                    let response = self.handle_request(request).await;
                    debug!(
                        response = response_summary(&response),
                        elapsed_ms = started.elapsed().as_millis(),
                        "daemon request completed"
                    );
                    trace!(response = ?response, "daemon response body");
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
        }
    }

    async fn handle_request(&self, request: ApiRequest) -> ApiResponse {
        match request {
            ApiRequest::GetUsage => {
                let today = chrono::Utc::now().date_naive();
                let recent_since = today
                    .checked_sub_days(chrono::Days::new(DASHBOARD_HISTORY_DAYS - 1))
                    .unwrap_or(today);
                match (
                    self.runtime.storage.latest_usage().await,
                    self.runtime.storage.accounts().await,
                    self.runtime
                        .storage
                        .daily_usage_dashboard(recent_since)
                        .await,
                ) {
                    (Ok(mut snapshots), Ok(accounts), Ok(history)) => {
                        merge_daily_usage_history(&mut snapshots, &history);
                        let snapshots = supported_visible_usage_snapshots(snapshots, &accounts);
                        let generated_at = chrono::Utc::now();
                        let since = generated_at - chrono::TimeDelta::days(FORECAST_HISTORY_DAYS);
                        let mut forecasts = Vec::new();
                        for snapshot in &snapshots {
                            let history = match self
                                .runtime
                                .storage
                                .recent_usage(
                                    &snapshot.provider_id,
                                    &snapshot.account_id,
                                    since,
                                    FORECAST_HISTORY_LIMIT,
                                )
                                .await
                            {
                                Ok(history) => history,
                                Err(err) => return storage_error(err),
                            };
                            forecasts.extend(forecast::forecast_snapshot(
                                snapshot,
                                &history,
                                generated_at,
                            ));
                        }
                        ApiResponse::Usage {
                            snapshots,
                            forecasts,
                        }
                    }
                    (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => storage_error(err),
                }
            }
            ApiRequest::Refresh { providers } => {
                let report = self.runtime.refresh.refresh(providers.as_deref()).await;
                ApiResponse::Refresh {
                    started_at: report.started_at,
                    finished_at: report.finished_at,
                    provider_results: report.provider_results,
                }
            }
            ApiRequest::GetProviderHealth => {
                match (
                    self.runtime.storage.provider_health().await,
                    self.runtime.storage.accounts().await,
                    self.runtime.visible_provider_ids().await,
                ) {
                    (Ok(health), Ok(accounts), Ok(visible_providers)) => {
                        ApiResponse::ProviderHealth {
                            health: visible_supported_provider_health(
                                health,
                                &accounts,
                                &visible_providers,
                            ),
                        }
                    }
                    (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => storage_error(err),
                }
            }
            ApiRequest::GetAccounts => match self.runtime.storage.accounts().await {
                Ok(accounts) => ApiResponse::Accounts {
                    accounts: supported_accounts(accounts),
                },
                Err(err) => storage_error(err),
            },
            ApiRequest::GetConfig => match self.runtime.config_response().await {
                Ok(config) => ApiResponse::Config { config },
                Err(err) => storage_error(err),
            },
            ApiRequest::UpdateConfig {
                poll_interval_seconds,
                providers,
                notifications,
            } => match self
                .runtime
                .update_config(poll_interval_seconds, providers, notifications)
                .await
            {
                Ok(config) => ApiResponse::Config { config },
                Err(err) => {
                    warn!(error = %err, "config update failed");
                    ApiResponse::error("invalid_config", err.to_string())
                }
            },
            ApiRequest::AddProviderAccount {
                provider_id,
                display_name,
            } => match self
                .runtime
                .add_provider_account(provider_id, display_name)
                .await
            {
                Ok(account) => ApiResponse::AddProviderAccount { account },
                Err(err) => {
                    warn!(error = %err, "add provider account failed");
                    ApiResponse::error("add_provider_account_failed", err.to_string())
                }
            },
            ApiRequest::UpdateAccount {
                account_id,
                display_name,
                hidden,
                collection_enabled,
            } => match self
                .runtime
                .update_account(account_id, display_name, hidden, collection_enabled)
                .await
            {
                Ok(account) => ApiResponse::Account { account },
                Err(err) => {
                    warn!(error = %err, "account update failed");
                    ApiResponse::error("account_update_failed", err.to_string())
                }
            },
            ApiRequest::RemoveAccount { account_id } => {
                match self.runtime.remove_account(account_id).await {
                    Ok(account) => ApiResponse::Account { account },
                    Err(err) => {
                        warn!(error = %err, "account remove failed");
                        ApiResponse::error("account_remove_failed", err.to_string())
                    }
                }
            }
            ApiRequest::DeleteAccount { account_id } => {
                match self.runtime.delete_account(account_id).await {
                    Ok(account_id) => ApiResponse::AccountDeleted { account_id },
                    Err(err) => {
                        warn!(error = %err, "account delete failed");
                        ApiResponse::error("account_delete_failed", err.to_string())
                    }
                }
            }
            ApiRequest::GetProviderSetup { provider_id } => {
                match self.runtime.provider_setup(provider_id).await {
                    Ok(setup) => ApiResponse::ProviderSetup { setup },
                    Err(err) => {
                        warn!(error = %err, "provider setup lookup failed");
                        ApiResponse::error("provider_setup_failed", err.to_string())
                    }
                }
            }
            ApiRequest::UpdateProviderSetup {
                provider_id,
                workspace_id,
            } => match self
                .runtime
                .update_provider_setup(provider_id, workspace_id)
                .await
            {
                Ok(setup) => ApiResponse::ProviderSetup { setup },
                Err(err) => {
                    warn!(error = %err, "provider setup update failed");
                    ApiResponse::error("provider_setup_update_failed", err.to_string())
                }
            },
            ApiRequest::RepairProvider {
                provider_id,
                account_id,
            } => match self.runtime.repair_provider(provider_id, account_id).await {
                Ok(action) => ApiResponse::ProviderAction { action },
                Err(err) => {
                    warn!(error = %err, "provider repair failed");
                    ApiResponse::error("provider_repair_failed", err.to_string())
                }
            },
            ApiRequest::LaunchProviderAccount { account_id } => {
                match self.runtime.launch_provider_account(account_id).await {
                    Ok(action) => ApiResponse::ProviderAction { action },
                    Err(err) => {
                        warn!(error = %err, "provider account launch failed");
                        ApiResponse::error("provider_account_launch_failed", err.to_string())
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestFrame<'a> {
    Line(&'a [u8]),
    TooLarge,
    EndOfStream,
}

async fn read_request_frame<'a, R>(
    reader: &mut R,
    line: &'a mut Vec<u8>,
) -> io::Result<RequestFrame<'a>>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                RequestFrame::EndOfStream
            } else {
                RequestFrame::Line(line)
            });
        }

        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_REQUEST_BYTES {
            reader.consume(take);
            return Ok(RequestFrame::TooLarge);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(RequestFrame::Line(line));
        }
    }
}

fn supported_visible_usage_snapshots(
    snapshots: Vec<UsageSnapshot>,
    accounts: &[Account],
) -> Vec<UsageSnapshot> {
    let hidden_accounts = hidden_account_ids(accounts);
    snapshots
        .into_iter()
        .filter(|snapshot| is_supported_provider(snapshot.provider_id.as_str()))
        .filter(|snapshot| !hidden_accounts.contains(snapshot.account_id.as_str()))
        .collect()
}

fn merge_daily_usage_history(snapshots: &mut [UsageSnapshot], history: &[StoredDailyUsageHistory]) {
    let history_by_account = history
        .iter()
        .map(|history| {
            (
                (history.provider_id.as_str(), history.account_id.as_str()),
                history,
            )
        })
        .collect::<HashMap<_, _>>();
    for snapshot in snapshots {
        let Some(matching) =
            history_by_account.get(&(snapshot.provider_id.as_str(), snapshot.account_id.as_str()))
        else {
            continue;
        };

        let rows = matching
            .recent
            .iter()
            .map(|row| {
                let mut value = serde_json::json!({
                    "date": row.date.to_string(),
                    "tokens": row.tokens,
                });
                if let Some(cost_usd) = row.cost_usd {
                    value["cost_usd"] = serde_json::json!(cost_usd);
                }
                value
            })
            .collect::<Vec<_>>();
        let source = matching
            .recent
            .last()
            .map(|row| row.source.as_str())
            .unwrap_or("persisted_daily_usage");
        let key = format!("{}_activity", snapshot.provider_id.as_str());
        if !snapshot.metadata.is_object() {
            snapshot.metadata = serde_json::json!({});
        }
        if !snapshot
            .metadata
            .get(&key)
            .is_some_and(serde_json::Value::is_object)
        {
            snapshot.metadata[&key] = serde_json::json!({});
        }
        let activity = &mut snapshot.metadata[&key];
        if activity.get("source").is_none() {
            activity["source"] = serde_json::json!(source);
        }
        activity["retained_history"] = serde_json::json!(true);
        activity["daily_bucket_count"] = serde_json::json!(matching.bucket_count);
        activity["history_days"] = serde_json::json!(DASHBOARD_HISTORY_DAYS);
        activity["by_day"] = serde_json::json!(rows);
        if snapshot.provider_id.as_str() == "codex" {
            replace_codex_activity_windows(snapshot, matching);
        }
    }
}

fn replace_codex_activity_windows(snapshot: &mut UsageSnapshot, history: &StoredDailyUsageHistory) {
    let today = chrono::Utc::now().date_naive();
    let lookback_start = today
        .checked_sub_days(chrono::Days::new(29))
        .unwrap_or(today);
    let today_tokens = history
        .recent
        .iter()
        .find(|row| row.date == today)
        .map(|row| row.tokens)
        .unwrap_or(0);
    let lookback_tokens = history
        .recent
        .iter()
        .filter(|row| row.date >= lookback_start && row.date <= today)
        .fold(0_u64, |total, row| total.saturating_add(row.tokens));
    let reported_lifetime_tokens = snapshot.metadata["codex_activity"]["lifetime_tokens"]
        .as_u64()
        .unwrap_or(0);
    let lifetime_tokens = history.total_tokens.max(reported_lifetime_tokens);

    snapshot.windows.retain(|window| {
        !matches!(
            window.window_id.as_str(),
            "codex_tokens_today" | "codex_tokens_30d" | "codex_tokens_lifetime"
        )
    });
    if today_tokens > 0 {
        snapshot.windows.push(activity_token_window(
            "codex_tokens_today",
            "Codex tokens today",
            today_tokens,
            UsageWindowKind::Daily,
        ));
    }
    if lookback_tokens > 0 {
        snapshot.windows.push(activity_token_window(
            "codex_tokens_30d",
            "Codex tokens 30 days",
            lookback_tokens,
            UsageWindowKind::Monthly,
        ));
    }
    if lifetime_tokens > 0 {
        snapshot.windows.push(activity_token_window(
            "codex_tokens_lifetime",
            "Codex lifetime tokens",
            lifetime_tokens,
            UsageWindowKind::Other("lifetime".to_string()),
        ));
    }
    snapshot.metadata["codex_activity"]["today_tokens"] = serde_json::json!(today_tokens);
    snapshot.metadata["codex_activity"]["lookback_days"] = serde_json::json!(30);
    snapshot.metadata["codex_activity"]["lookback_tokens"] = serde_json::json!(lookback_tokens);
    snapshot.metadata["codex_activity"]["lifetime_tokens"] = serde_json::json!(lifetime_tokens);
}

fn activity_token_window(
    window_id: &str,
    label: &str,
    tokens: u64,
    kind: UsageWindowKind,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
        used: Some(UsageAmount {
            value: tokens as f64,
            unit: UsageUnit::Tokens,
        }),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

fn visible_supported_provider_health(
    health: Vec<ProviderHealth>,
    accounts: &[Account],
    visible_providers: &BTreeSet<String>,
) -> Vec<ProviderHealth> {
    let hidden_accounts = hidden_account_ids(accounts);
    health
        .into_iter()
        .filter(|row| is_supported_provider(row.provider_id.as_str()))
        .filter(|row| visible_providers.contains(row.provider_id.as_str()))
        .filter(|row| {
            row.account_id
                .as_ref()
                .is_none_or(|id| !hidden_accounts.contains(id.as_str()))
        })
        .collect()
}

fn supported_accounts(accounts: Vec<Account>) -> Vec<Account> {
    accounts
        .into_iter()
        .filter(|account| is_supported_provider(account.provider_id.as_str()))
        .collect()
}

fn is_supported_provider(provider_id: &str) -> bool {
    matches!(provider_id, "codex" | "claude" | "opencode_go")
}

fn hidden_account_ids(accounts: &[Account]) -> BTreeSet<&str> {
    accounts
        .iter()
        .filter(|account| account.hidden)
        .map(|account| account.id.as_str())
        .collect()
}

fn storage_error(err: anyhow::Error) -> ApiResponse {
    warn!(error = %err, "storage request failed");
    ApiResponse::error("storage_error", err.to_string())
}

fn response_summary(response: &ApiResponse) -> &'static str {
    match response {
        ApiResponse::Usage { .. } => "usage",
        ApiResponse::Refresh { .. } => "refresh",
        ApiResponse::ProviderHealth { .. } => "provider_health",
        ApiResponse::Accounts { .. } => "accounts",
        ApiResponse::Config { .. } => "config",
        ApiResponse::AddProviderAccount { .. } => "add_provider_account",
        ApiResponse::Account { .. } => "account",
        ApiResponse::AccountDeleted { .. } => "account_deleted",
        ApiResponse::ProviderSetup { .. } => "provider_setup",
        ApiResponse::ProviderAction { .. } => "provider_action",
        ApiResponse::Error { .. } => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polling::RefreshCoordinator;
    use crate::storage::StoredDailyUsage;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
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
            notifications: Default::default(),
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

    #[test]
    fn merges_retained_daily_usage_and_replaces_local_token_windows() {
        let today = chrono::Utc::now().date_naive();
        let account_id = usage_core::AccountId::new("account");
        let provider_id = ProviderId::new("codex");
        let mut snapshots = vec![UsageSnapshot {
            provider_id: provider_id.clone(),
            account_id: account_id.clone(),
            collected_at: chrono::Utc::now(),
            windows: vec![activity_token_window(
                "codex_tokens_today",
                "local fallback",
                1,
                UsageWindowKind::Daily,
            )],
            metadata: serde_json::json!({
                "codex_activity": {"lifetime_tokens": 40}
            }),
        }];
        let history = vec![StoredDailyUsageHistory {
            provider_id: provider_id.clone(),
            account_id: account_id.clone(),
            bucket_count: 2,
            total_tokens: 40,
            recent: vec![StoredDailyUsage {
                provider_id,
                account_id,
                date: today,
                tokens: 25,
                cost_usd: None,
                source: "codex_account_usage".to_string(),
            }],
        }];

        merge_daily_usage_history(&mut snapshots, &history);

        assert_eq!(
            snapshots[0].metadata["codex_activity"]["by_day"][0]["tokens"],
            25
        );
        assert_eq!(
            snapshots[0].metadata["codex_activity"]["lifetime_tokens"],
            40
        );
        assert_eq!(
            snapshots[0].metadata["codex_activity"]["daily_bucket_count"],
            2
        );
        let today_window = snapshots[0]
            .windows
            .iter()
            .find(|window| window.window_id == "codex_tokens_today")
            .unwrap();
        assert_eq!(today_window.used.as_ref().unwrap().value, 25.0);
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
    async fn bounds_request_frames_before_allocating_the_entire_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES + 1];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, input.as_slice());
        let mut line = Vec::new();

        let frame = read_request_frame(&mut reader, &mut line).await.unwrap();

        assert_eq!(frame, RequestFrame::TooLarge);
        assert!(line.len() <= MAX_REQUEST_BYTES);
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
        assert_eq!(
            std::fs::metadata(&env.socket_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
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
            crate::config::ProviderConfig {
                enabled: true,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
            },
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
            r#"{"method":"update_config","poll_interval_seconds":120,"providers":{"codex":{"enabled":false}},"notifications":{"enabled":false}}"#,
        )
        .await;

        match response {
            ApiResponse::Config { config } => {
                assert_eq!(config.poll_interval_seconds, 120);
                assert!(!config.notifications.enabled);
                assert_eq!(config.enabled_providers, Vec::<ProviderId>::new());
                assert!(!config.providers.contains_key("codex"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(persisted["poll_interval_seconds"], 120);
        assert_eq!(persisted["notifications"]["enabled"], false);
        assert_eq!(persisted["providers"]["codex"]["enabled"], false);

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn serves_disabled_provider_when_database_has_data() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            crate::config::ProviderConfig {
                enabled: false,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
            },
        );
        let env = test_env(providers);
        env.runtime
            .storage
            .upsert_account(
                &ProviderId::new("codex"),
                "external-account",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let server = SocketServer::new(env.runtime.clone());

        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let response = request_line(&env.socket_path, r#"{"method":"get_config"}"#).await;

        match response {
            ApiResponse::Config { config } => {
                assert_eq!(config.enabled_providers, Vec::<ProviderId>::new());
                assert!(!config.providers["codex"].enabled);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[test]
    fn filters_health_to_visible_supported_providers() {
        let mut visible = BTreeSet::new();
        visible.insert("codex".to_string());
        let now = chrono::Utc::now();

        let filtered = visible_supported_provider_health(
            vec![
                ProviderHealth {
                    provider_id: ProviderId::new("codex"),
                    account_id: None,
                    status: usage_core::ProviderHealthStatus::Disabled,
                    collection_mode: None,
                    last_success_at: None,
                    last_failure_at: None,
                    last_error_code: None,
                    last_error_message: None,
                    updated_at: now,
                },
                ProviderHealth {
                    provider_id: ProviderId::new("claude"),
                    account_id: None,
                    status: usage_core::ProviderHealthStatus::Disabled,
                    collection_mode: None,
                    last_success_at: None,
                    last_failure_at: None,
                    last_error_code: None,
                    last_error_message: None,
                    updated_at: now,
                },
                ProviderHealth {
                    provider_id: ProviderId::new("opencode"),
                    account_id: None,
                    status: usage_core::ProviderHealthStatus::Ok,
                    collection_mode: None,
                    last_success_at: None,
                    last_failure_at: None,
                    last_error_code: None,
                    last_error_message: None,
                    updated_at: now,
                },
            ],
            &[],
            &visible,
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider_id, ProviderId::new("codex"));
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

    #[tokio::test]
    async fn serves_safe_provider_profile_options() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            crate::config::ProviderConfig {
                enabled: true,
                profiles: vec![crate::config::ProviderProfileConfig {
                    id: Some("work".to_string()),
                    display_name: Some("Work".to_string()),
                    ..Default::default()
                }],
                cookie_header: None,
                workspace_id: None,
            },
        );
        let env = test_env(providers);
        let server = SocketServer::new(env.runtime.clone());
        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let response = request_line(
            &env.socket_path,
            r#"{"method":"get_provider_setup","provider_id":"codex"}"#,
        )
        .await;
        match response {
            ApiResponse::ProviderSetup { setup } => {
                assert_eq!(setup.provider_id, ProviderId::new("codex"));
                assert_eq!(setup.profiles.len(), 1);
                assert_eq!(setup.profiles[0].id, "work");
                assert_eq!(setup.profiles[0].display_name.as_deref(), Some("Work"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn account_rename_updates_profile_config() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            crate::config::ProviderConfig {
                enabled: true,
                profiles: vec![crate::config::ProviderProfileConfig {
                    id: Some("work".to_string()),
                    display_name: Some("Old name".to_string()),
                    ..Default::default()
                }],
                cookie_header: None,
                workspace_id: None,
            },
        );
        let env = test_env(providers);
        let account = env
            .runtime
            .storage
            .upsert_account(
                &ProviderId::new("codex"),
                "external",
                Some("work"),
                Some("Old name"),
                None,
            )
            .await
            .unwrap();
        let server = SocketServer::new(env.runtime.clone());
        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let request = format!(
            r#"{{"method":"update_account","account_id":"{}","display_name":"New name"}}"#,
            account.id
        );
        let response = request_line(&env.socket_path, &request).await;
        match response {
            ApiResponse::Account { account } => {
                assert_eq!(account.display_name.as_deref(), Some("New name"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(env.root.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted["providers"]["codex"]["profiles"][0]["display_name"],
            "New name"
        );

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn permanent_delete_removes_data_and_tombstones_profile() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            crate::config::ProviderConfig {
                enabled: true,
                profiles: vec![crate::config::ProviderProfileConfig {
                    id: Some("work".to_string()),
                    display_name: Some("Work".to_string()),
                    ..Default::default()
                }],
                cookie_header: None,
                workspace_id: None,
            },
        );
        let env = test_env(providers);
        let account = env
            .runtime
            .storage
            .upsert_account(
                &ProviderId::new("codex"),
                "external",
                Some("work"),
                Some("Work"),
                None,
            )
            .await
            .unwrap();
        let server = SocketServer::new(env.runtime.clone());
        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let request = format!(
            r#"{{"method":"delete_account","account_id":"{}"}}"#,
            account.id
        );
        let response = request_line(&env.socket_path, &request).await;
        match response {
            ApiResponse::AccountDeleted { account_id } => assert_eq!(account_id, account.id),
            other => panic!("unexpected response: {other:?}"),
        }
        assert!(env.runtime.storage.accounts().await.unwrap().is_empty());

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(env.root.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["providers"]["codex"]["enabled"], false);
        assert_eq!(
            persisted["providers"]["codex"]["profiles"][0]["deleted"],
            true
        );

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
