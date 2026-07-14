use std::{
    collections::BTreeSet,
    io,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};
use tracing::{debug, trace, warn};
use usage_core::{
    Account, AccountId, ApiErrorCode, ApiRequest, ApiResponse, ProviderHealth, RequestEnvelope,
    ResponseEnvelope, ServerInfo, StateSnapshot, UsageDashboardSummary, UsageForecast,
    UsageSnapshot, UsageWindowProvenance, API_VERSION, MAX_RESPONSE_BYTES,
};

use crate::{daemon::DaemonRuntime, dashboard, forecast};

const MAX_CLIENT_CONNECTIONS: usize = 64;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const DASHBOARD_HISTORY_DAYS: u64 = 30;
const FORECAST_HISTORY_DAYS: i64 = 35;
const FORECAST_HISTORY_LIMIT: usize = 1_024;

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
        let mut response_bytes = Vec::with_capacity(8 * 1024);

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
                    let response = ResponseEnvelope::error(
                        ApiErrorCode::RequestTooLarge,
                        format!("request exceeds the {MAX_REQUEST_BYTES}-byte limit"),
                    );
                    response_bytes.clear();
                    write_response(&mut writer, &response, &mut response_bytes).await?;
                }
                return Ok(());
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            let response = match decode_request(line) {
                Ok(request) => {
                    debug!(request = ?request, "daemon request received");
                    let started = Instant::now();
                    let response = self.handle_request(request).await;
                    debug!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "daemon request completed"
                    );
                    trace!(response = ?response, "daemon response body");
                    ResponseEnvelope::new(response)
                }
                Err(response) => {
                    warn!(response = ?response, "invalid daemon request");
                    ResponseEnvelope::new(response)
                }
            };
            if write_response(&mut writer, &response, &mut response_bytes).await? {
                return Ok(());
            }
        }
    }

    async fn handle_request(&self, request: ApiRequest) -> ApiResponse {
        match request {
            ApiRequest::GetServerInfo => ApiResponse::ServerInfo {
                server: ServerInfo::current(crate::runtime::provider_registry::descriptors()),
            },
            ApiRequest::GetState => self.state_response().await,
            ApiRequest::GetUsage => {
                let generated_at = chrono::Utc::now();
                let today = generated_at.with_timezone(&chrono::Local).date_naive();
                let recent_since = today
                    .checked_sub_days(chrono::Days::new(DASHBOARD_HISTORY_DAYS - 1))
                    .unwrap_or(today);
                let since = generated_at - chrono::TimeDelta::days(FORECAST_HISTORY_DAYS);
                match self
                    .runtime
                    .storage
                    .usage_dashboard(recent_since, since, FORECAST_HISTORY_LIMIT)
                    .await
                {
                    Ok(stored) => {
                        let usage = build_usage_view(stored, generated_at);
                        ApiResponse::Usage {
                            snapshots: usage.snapshots,
                            dashboard: usage.dashboard,
                            forecasts: usage.forecasts,
                            window_provenance: usage.window_provenance,
                        }
                    }
                    Err(err) => storage_error(err),
                }
            }
            ApiRequest::Refresh { providers } => match validated_refresh_scope(providers) {
                Ok(providers) => {
                    let started = self
                        .runtime
                        .refresh
                        .start_refresh(providers, usage_core::RefreshTrigger::Manual)
                        .await;
                    ApiResponse::RefreshStarted {
                        job: started.job,
                        coalesced: started.coalesced,
                    }
                }
                Err(error) => error,
            },
            ApiRequest::GetRefreshJob { job_id } => {
                match self.runtime.refresh.get_refresh_job(&job_id).await {
                    Some(job) => ApiResponse::RefreshJob { job },
                    None => ApiResponse::error(
                        ApiErrorCode::UnknownRefreshJob,
                        format!("unknown refresh job: {job_id}"),
                    ),
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
            ApiRequest::GetPendingNotifications => {
                match self.runtime.storage.pending_notifications().await {
                    Ok(notifications) => ApiResponse::PendingNotifications { notifications },
                    Err(err) => storage_error(err),
                }
            }
            ApiRequest::AcknowledgeNotifications { ids } => {
                match self.runtime.storage.acknowledge_notifications(&ids).await {
                    Ok(()) => ApiResponse::NotificationsAcknowledged { ids },
                    Err(err) => storage_error(err),
                }
            }
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
                    ApiResponse::error(ApiErrorCode::InvalidArgument, err.to_string())
                }
            },
            ApiRequest::AddProviderAccount {
                provider_id,
                display_name,
            } => {
                if let Some(error) = provider_validation_error(&provider_id) {
                    error
                } else if crate::runtime::provider_registry::find(provider_id.as_str())
                    .is_none_or(|provider| provider.add_account_handler().is_none())
                {
                    ApiResponse::error(
                        ApiErrorCode::UnsupportedOperation,
                        format!("adding accounts is not supported for {provider_id}"),
                    )
                } else {
                    match self
                        .runtime
                        .add_provider_account(provider_id, display_name)
                        .await
                    {
                        Ok(account) => ApiResponse::AddProviderAccount { account },
                        Err(err) => {
                            warn!(error = %err, "add provider account failed");
                            ApiResponse::error(ApiErrorCode::Internal, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::UpdateAccount {
                account_id,
                display_name,
                hidden,
                collection_enabled,
            } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    match self
                        .runtime
                        .update_account(account_id, display_name, hidden, collection_enabled)
                        .await
                    {
                        Ok(account) => ApiResponse::Account { account },
                        Err(err) => {
                            warn!(error = %err, "account update failed");
                            ApiResponse::error(ApiErrorCode::Internal, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::RemoveAccount { account_id } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    match self.runtime.remove_account(account_id).await {
                        Ok(account) => ApiResponse::Account { account },
                        Err(err) => {
                            warn!(error = %err, "account remove failed");
                            ApiResponse::error(ApiErrorCode::Internal, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::DeleteAccount { account_id } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    match self.runtime.delete_account(account_id).await {
                        Ok(account_id) => ApiResponse::AccountDeleted { account_id },
                        Err(err) => {
                            warn!(error = %err, "account delete failed");
                            ApiResponse::error(ApiErrorCode::Internal, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::GetProviderSetup { provider_id } => {
                if let Some(error) = provider_validation_error(&provider_id) {
                    error
                } else {
                    match self.runtime.provider_setup(provider_id).await {
                        Ok(setup) => ApiResponse::ProviderSetup { setup },
                        Err(err) => {
                            warn!(error = %err, "provider setup lookup failed");
                            ApiResponse::error(ApiErrorCode::Internal, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::UpdateProviderSetup {
                provider_id,
                mut settings,
                workspace_id,
            } => {
                if let Some(error) = provider_validation_error(&provider_id) {
                    error
                } else if crate::runtime::provider_registry::find(provider_id.as_str())
                    .is_none_or(|provider| provider.setup_handler().is_none())
                {
                    ApiResponse::error(
                        ApiErrorCode::UnsupportedOperation,
                        format!("setup is not supported for {provider_id}"),
                    )
                } else {
                    let supports_legacy_workspace_setup = crate::runtime::provider_registry::find(
                        provider_id.as_str(),
                    )
                    .is_some_and(|provider| provider.descriptor().capabilities.workspace_setup);
                    if supports_legacy_workspace_setup && settings.is_empty() {
                        settings.insert("workspace_id".to_string(), workspace_id);
                    } else if workspace_id.is_some() {
                        return ApiResponse::error(
                            ApiErrorCode::InvalidArgument,
                            "workspace_id is only supported by workspace-based provider setup; otherwise send provider setup values through settings",
                        );
                    }
                    match self
                        .runtime
                        .update_provider_setup(provider_id, settings)
                        .await
                    {
                        Ok(setup) => ApiResponse::ProviderSetup { setup },
                        Err(err) => {
                            warn!(error = %err, "provider setup update failed");
                            ApiResponse::error(ApiErrorCode::InvalidArgument, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::RepairProvider {
                provider_id,
                account_id,
            } => {
                let account_error = match account_id.as_ref() {
                    Some(account_id) => self.account_validation_error(account_id).await,
                    None => None,
                };
                if let Some(error) = provider_validation_error(&provider_id) {
                    error
                } else if crate::runtime::provider_registry::find(provider_id.as_str())
                    .is_none_or(|provider| provider.repair_handler().is_none())
                {
                    ApiResponse::error(
                        ApiErrorCode::UnsupportedOperation,
                        format!("repair is not supported for {provider_id}"),
                    )
                } else if let Some(error) = account_error {
                    error
                } else {
                    match self.runtime.repair_provider(provider_id, account_id).await {
                        Ok(action) => ApiResponse::ProviderAction { action },
                        Err(err) => {
                            warn!(error = %err, "provider repair failed");
                            ApiResponse::error(ApiErrorCode::Internal, err.to_string())
                        }
                    }
                }
            }
            ApiRequest::LaunchProviderAccount { account_id } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    match self.runtime.launch_provider_account(account_id).await {
                        Ok(action) => ApiResponse::ProviderAction { action },
                        Err(err) => {
                            warn!(error = %err, "provider account launch failed");
                            ApiResponse::error(ApiErrorCode::UnsupportedOperation, err.to_string())
                        }
                    }
                }
            }
        }
    }

    async fn state_response(&self) -> ApiResponse {
        let generated_at = chrono::Utc::now();
        let today = generated_at.with_timezone(&chrono::Local).date_naive();
        let recent_since = today
            .checked_sub_days(chrono::Days::new(DASHBOARD_HISTORY_DAYS - 1))
            .unwrap_or(today);
        let forecast_since = generated_at - chrono::TimeDelta::days(FORECAST_HISTORY_DAYS);
        let stored = match self
            .runtime
            .storage
            .usage_dashboard(recent_since, forecast_since, FORECAST_HISTORY_LIMIT)
            .await
        {
            Ok(stored) => stored,
            Err(err) => return storage_error(err),
        };

        let data_provider_ids = stored
            .accounts
            .iter()
            .map(|account| account.provider_id.as_str().to_string())
            .chain(
                stored
                    .snapshots
                    .iter()
                    .map(|snapshot| snapshot.provider_id.as_str().to_string()),
            )
            .collect();
        let (config, visible_provider_ids) = self
            .runtime
            .config_response_for_provider_data(data_provider_ids)
            .await;
        let usage = build_usage_view_from_parts(
            stored.snapshots,
            &stored.accounts,
            &stored.daily_usage,
            &stored.forecast_histories,
            generated_at,
        );
        let health = visible_supported_provider_health(
            stored.health,
            &stored.accounts,
            &visible_provider_ids,
        );
        let accounts = supported_accounts(stored.accounts);

        ApiResponse::State {
            state: StateSnapshot {
                generated_at,
                server: ServerInfo::current(crate::runtime::provider_registry::descriptors()),
                config,
                accounts,
                health,
                snapshots: usage.snapshots,
                dashboard: usage.dashboard,
                forecasts: usage.forecasts,
                window_provenance: usage.window_provenance,
            },
        }
    }

    async fn account_validation_error(&self, account_id: &AccountId) -> Option<ApiResponse> {
        match self.runtime.storage.account(account_id).await {
            Ok(Some(_)) => None,
            Ok(None) => Some(ApiResponse::error(
                ApiErrorCode::UnknownAccount,
                format!("unknown account: {account_id}"),
            )),
            Err(err) => Some(storage_error(err)),
        }
    }
}

struct UsageView {
    snapshots: Vec<UsageSnapshot>,
    dashboard: UsageDashboardSummary,
    forecasts: Vec<UsageForecast>,
    window_provenance: Vec<UsageWindowProvenance>,
}

fn build_usage_view(
    stored: crate::storage::StoredUsageDashboard,
    generated_at: chrono::DateTime<chrono::Utc>,
) -> UsageView {
    build_usage_view_from_parts(
        stored.snapshots,
        &stored.accounts,
        &stored.daily_usage,
        &stored.forecast_histories,
        generated_at,
    )
}

fn build_usage_view_from_parts(
    snapshots: Vec<UsageSnapshot>,
    accounts: &[Account],
    daily_usage: &[crate::storage::StoredDailyUsageHistory],
    forecast_histories: &std::collections::HashMap<
        (usage_core::ProviderId, AccountId),
        crate::storage::StoredForecastHistory,
    >,
    generated_at: chrono::DateTime<chrono::Utc>,
) -> UsageView {
    let snapshots = supported_visible_usage_snapshots(snapshots, accounts);
    let empty_history = crate::storage::StoredForecastHistory::default();
    let forecasts = snapshots
        .iter()
        .flat_map(|snapshot| {
            let history = forecast_histories
                .get(&(snapshot.provider_id.clone(), snapshot.account_id.clone()))
                .unwrap_or(&empty_history);
            forecast::forecast_snapshot(snapshot, history, generated_at)
        })
        .collect();
    let window_provenance = snapshots
        .iter()
        .flat_map(usage_core::UsageSnapshot::windows_provenance)
        .collect();
    let dashboard = dashboard::build_usage_dashboard(&snapshots, daily_usage);
    UsageView {
        snapshots,
        dashboard,
        forecasts,
        window_provenance,
    }
}

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &ResponseEnvelope,
    buffer: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    let close_after_write = encode_response(response, buffer, MAX_RESPONSE_BYTES)?;
    write_all_with_timeout(writer, buffer, CLIENT_WRITE_TIMEOUT).await?;
    Ok(close_after_write)
}

async fn write_all_with_timeout<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    timeout: Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, writer.write_all(bytes))
        .await
        .map_err(|_| anyhow::anyhow!("timed out writing daemon response"))??;
    Ok(())
}

fn encode_response(
    response: &ResponseEnvelope,
    buffer: &mut Vec<u8>,
    max_response_bytes: usize,
) -> serde_json::Result<bool> {
    buffer.clear();
    let (result, limit_exceeded) = {
        let mut writer = BoundedResponseWriter::new(buffer, max_response_bytes.saturating_sub(1));
        let result = serde_json::to_writer(&mut writer, response);
        (result, writer.limit_exceeded)
    };
    if !limit_exceeded {
        result?;
        buffer.push(b'\n');
        return Ok(false);
    }

    buffer.clear();
    serde_json::to_writer(
        &mut *buffer,
        &ResponseEnvelope::error(
            ApiErrorCode::Internal,
            format!("daemon response exceeds the {max_response_bytes}-byte limit"),
        ),
    )?;
    buffer.push(b'\n');
    Ok(true)
}

struct BoundedResponseWriter<'a> {
    buffer: &'a mut Vec<u8>,
    max_bytes: usize,
    limit_exceeded: bool,
}

impl<'a> BoundedResponseWriter<'a> {
    fn new(buffer: &'a mut Vec<u8>, max_bytes: usize) -> Self {
        Self {
            buffer,
            max_bytes,
            limit_exceeded: false,
        }
    }
}

impl io::Write for BoundedResponseWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.buffer.len()) {
            self.limit_exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "daemon response exceeds size limit",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decode_request(line: &[u8]) -> Result<ApiRequest, ApiResponse> {
    match serde_json::from_slice::<RequestEnvelope>(line) {
        Ok(envelope) if envelope.api_version == API_VERSION => return Ok(envelope.request),
        Ok(envelope) => {
            return Err(ApiResponse::error(
                ApiErrorCode::IncompatibleProtocol,
                format!(
                    "unsupported api_version {}; server requires {API_VERSION}",
                    envelope.api_version
                ),
            ));
        }
        Err(_) => {}
    }

    classify_invalid_request(line)
}

/// Retains the protocol's precise error categories on the uncommon invalid
/// request path without making valid requests allocate an intermediate JSON DOM.
fn classify_invalid_request(line: &[u8]) -> Result<ApiRequest, ApiResponse> {
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|err| {
        ApiResponse::error(
            ApiErrorCode::InvalidJson,
            format!("invalid request JSON: {err}"),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        ApiResponse::error(
            ApiErrorCode::InvalidRequest,
            "request must be a JSON object",
        )
    })?;
    let version = object
        .get("api_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            ApiResponse::error(
                ApiErrorCode::IncompatibleProtocol,
                format!("request must declare api_version {API_VERSION}"),
            )
        })?;
    if version != u64::from(API_VERSION) {
        return Err(ApiResponse::error(
            ApiErrorCode::IncompatibleProtocol,
            format!("unsupported api_version {version}; server requires {API_VERSION}"),
        ));
    }
    let method = object
        .get("method")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ApiResponse::error(
                ApiErrorCode::InvalidRequest,
                "request method must be a string",
            )
        })?
        .to_string();
    if !ApiRequest::supports_method(&method) {
        return Err(ApiResponse::error(
            ApiErrorCode::UnsupportedMethod,
            format!("unsupported method: {method}"),
        ));
    }

    serde_json::from_value::<RequestEnvelope>(value)
        .map(|envelope| envelope.request)
        .map_err(|err| {
            ApiResponse::error(
                ApiErrorCode::InvalidRequest,
                format!("invalid {method} request: {err}"),
            )
        })
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
    crate::runtime::provider_registry::is_supported(provider_id)
}

fn provider_validation_error(provider_id: &usage_core::ProviderId) -> Option<ApiResponse> {
    (!is_supported_provider(provider_id.as_str())).then(|| {
        ApiResponse::error(
            ApiErrorCode::UnknownProvider,
            format!("unknown provider: {provider_id}"),
        )
    })
}

fn validated_refresh_scope(
    providers: Option<Vec<usage_core::ProviderId>>,
) -> Result<Option<Vec<usage_core::ProviderId>>, ApiResponse> {
    let Some(providers) = providers else {
        return Ok(None);
    };
    if providers.is_empty() {
        return Err(ApiResponse::error(
            ApiErrorCode::InvalidArgument,
            "refresh providers must not be empty; omit providers to refresh all",
        ));
    }
    if let Some(provider_id) = providers
        .iter()
        .find(|provider_id| !is_supported_provider(provider_id.as_str()))
    {
        return Err(provider_validation_error(provider_id)
            .expect("unsupported provider must produce a validation error"));
    }
    Ok(usage_core::RefreshScope::providers(providers).providers)
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
    ApiResponse::error(ApiErrorCode::StorageUnavailable, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;
    use crate::polling::RefreshCoordinator;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::{timeout, Duration};
    use usage_core::ProviderId;
    use uuid::Uuid;

    fn benchmark(name: &str, iterations: u32, mut operation: impl FnMut() -> usize) {
        for _ in 0..iterations.min(100) {
            std::hint::black_box(operation());
        }
        let started = std::time::Instant::now();
        let mut checksum = 0;
        for _ in 0..iterations {
            checksum ^= std::hint::black_box(operation());
        }
        let elapsed = started.elapsed();
        println!(
            "BENCH {name}: {:.2} us/iter ({iterations} iterations, checksum={checksum})",
            elapsed.as_secs_f64() * 1_000_000.0 / f64::from(iterations)
        );
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_daemon_response_pipeline() {
        let request = serde_json::to_vec(&RequestEnvelope::new(
            ApiRequest::AcknowledgeNotifications {
                ids: (0..2_048).collect(),
            },
        ))
        .unwrap();
        benchmark(
            "daemon.decode_request.2048_ids",
            10_000,
            || match decode_request(std::hint::black_box(&request)).unwrap() {
                ApiRequest::AcknowledgeNotifications { ids } => ids.len(),
                _ => unreachable!(),
            },
        );

        let now = chrono::Utc::now();
        let accounts = (0..256)
            .map(|index| Account {
                id: AccountId::new(format!("account-{index}")),
                provider_id: usage_core::ProviderId::new("codex"),
                external_account_id: format!("external-{index}"),
                profile_id: Some(format!("profile-{index}")),
                display_name: Some(format!("Benchmark account {index}")),
                display_name_source: Default::default(),
                email: Some(format!("account-{index}@example.com")),
                hidden: false,
                collection_enabled: true,
                created_at: now,
                updated_at: now,
            })
            .collect();
        let response = ResponseEnvelope::new(ApiResponse::Accounts { accounts });
        let mut buffer = Vec::with_capacity(64 * 1024);
        benchmark("daemon.encode_response.256_accounts", 5_000, || {
            let close = encode_response(
                std::hint::black_box(&response),
                &mut buffer,
                MAX_RESPONSE_BYTES,
            )
            .unwrap();
            assert!(!close);
            buffer.len()
        });
    }

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
        let mut request_value: serde_json::Value = serde_json::from_str(request).unwrap();
        request_value["api_version"] = serde_json::json!(API_VERSION);
        request_value_line(socket_path, request_value).await
    }

    async fn request_value_line(
        socket_path: &Path,
        request_value: serde_json::Value,
    ) -> ApiResponse {
        let mut stream = UnixStream::connect(socket_path).await.unwrap();
        stream
            .write_all(&serde_json::to_vec(&request_value).unwrap())
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();

        let mut lines = BufReader::new(stream).lines();
        let response = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str::<ResponseEnvelope>(&response)
            .unwrap()
            .response
    }

    #[tokio::test]
    async fn returns_typed_protocol_and_method_errors() {
        let env = test_env(BTreeMap::new());
        let server = SocketServer::new(env.runtime.clone());
        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });
        wait_for_socket(&env.socket_path).await;

        let unsupported = request_value_line(
            &env.socket_path,
            serde_json::json!({"api_version": API_VERSION, "method": "old_refresh"}),
        )
        .await;
        let ApiResponse::Error { error } = unsupported else {
            panic!("unexpected unsupported-method response")
        };
        assert_eq!(error.code, ApiErrorCode::UnsupportedMethod);

        let incompatible = request_value_line(
            &env.socket_path,
            serde_json::json!({"api_version": 1, "method": "get_config"}),
        )
        .await;
        let ApiResponse::Error { error } = incompatible else {
            panic!("unexpected incompatible-protocol response")
        };
        assert_eq!(error.code, ApiErrorCode::IncompatibleProtocol);

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
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

    #[test]
    fn replaces_oversized_responses_with_a_bounded_error_and_closes() {
        let response = ResponseEnvelope::new(ApiResponse::ServerInfo {
            server: ServerInfo::current(crate::runtime::provider_registry::descriptors()),
        });
        let mut buffer = Vec::new();

        let close = encode_response(&response, &mut buffer, 512).unwrap();

        assert!(close);
        assert!(buffer.len() <= 512);
        let encoded: ResponseEnvelope = serde_json::from_slice(&buffer).unwrap();
        let ApiResponse::Error { error } = encoded.response else {
            panic!("expected a bounded error response")
        };
        assert_eq!(error.code, ApiErrorCode::Internal);
    }

    #[tokio::test]
    async fn times_out_when_a_client_stops_reading_a_response() {
        let (mut writer, _reader) = tokio::io::duplex(1);

        let error = write_all_with_timeout(&mut writer, &[0; 1024], Duration::from_millis(10))
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("timed out writing daemon response"));
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
                assert!(config.providers.is_empty());
            }
            other => panic!("unexpected response: {other:?}"),
        }

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn rejects_unknown_and_empty_explicit_refresh_scopes() {
        let env = test_env(BTreeMap::new());
        let server = SocketServer::new(env.runtime.clone());

        let unknown = server
            .handle_request(ApiRequest::Refresh {
                providers: Some(vec![ProviderId::new("definitely_unknown")]),
            })
            .await;
        let ApiResponse::Error { error } = unknown else {
            panic!("unexpected response: {unknown:?}")
        };
        assert_eq!(error.code, ApiErrorCode::UnknownProvider);

        let empty = server
            .handle_request(ApiRequest::Refresh {
                providers: Some(Vec::new()),
            })
            .await;
        let ApiResponse::Error { error } = empty else {
            panic!("unexpected response: {empty:?}")
        };
        assert_eq!(error.code, ApiErrorCode::InvalidArgument);

        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn serves_fixture_accounts_usage_and_notifications_over_socket() {
        let env = test_env(BTreeMap::new());
        crate::fixtures::seed(
            &env.runtime.storage,
            crate::fixtures::FixtureScenario::Notifications,
        )
        .await
        .unwrap();
        let server = SocketServer::new(env.runtime.clone());
        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });
        wait_for_socket(&env.socket_path).await;

        let accounts = request_line(&env.socket_path, r#"{"method":"get_accounts"}"#).await;
        let ApiResponse::Accounts { accounts } = accounts else {
            panic!("unexpected accounts response")
        };
        assert_eq!(accounts.len(), 6);
        for provider_id in ["claude", "codex"] {
            assert_eq!(
                accounts
                    .iter()
                    .filter(|account| account.provider_id.as_str() == provider_id)
                    .count(),
                2
            );
        }

        let usage = request_line(&env.socket_path, r#"{"method":"get_usage"}"#).await;
        let ApiResponse::Usage {
            snapshots,
            dashboard,
            forecasts,
            ..
        } = usage
        else {
            panic!("unexpected usage response")
        };
        assert_eq!(snapshots.len(), 6);
        assert!(!forecasts.is_empty());
        assert!(dashboard.accounts.iter().any(|account| {
            account
                .activity
                .as_ref()
                .is_some_and(|activity| !activity.days.is_empty() && activity.lookback_tokens > 0)
        }));

        let state = request_line(&env.socket_path, r#"{"method":"get_state"}"#).await;
        let ApiResponse::State { state } = state else {
            panic!("unexpected state response")
        };
        assert_eq!(state.accounts.len(), 6);
        assert_eq!(state.snapshots.len(), 6);
        assert_eq!(state.server.api_version, API_VERSION);
        assert!(state
            .server
            .capabilities
            .contains(&usage_core::ApiCapability::CombinedState));

        let pending = request_line(
            &env.socket_path,
            r#"{"method":"get_pending_notifications"}"#,
        )
        .await;
        let ApiResponse::PendingNotifications { notifications } = pending else {
            panic!("unexpected notifications response")
        };
        assert!(notifications.len() >= 6);

        server_task.abort();
        let _ = std::fs::remove_file(env.socket_path);
        let _ = std::fs::remove_dir_all(env.root);
    }

    #[tokio::test]
    async fn serves_and_acknowledges_pending_notifications_over_socket() {
        let env = test_env(BTreeMap::new());
        env.runtime
            .storage
            .enqueue_notification("Usage low", "5% remaining")
            .await
            .unwrap();
        let server = SocketServer::new(env.runtime.clone());
        let server_task = tokio::spawn({
            let socket_path = env.socket_path.clone();
            async move { server.run(&socket_path).await }
        });

        wait_for_socket(&env.socket_path).await;
        let response = request_line(
            &env.socket_path,
            r#"{"method":"get_pending_notifications"}"#,
        )
        .await;
        let ApiResponse::PendingNotifications { notifications } = response else {
            panic!("unexpected response")
        };
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].title, "Usage low");

        let response = request_line(
            &env.socket_path,
            &format!(
                r#"{{"method":"acknowledge_notifications","ids":[{}]}}"#,
                notifications[0].id
            ),
        )
        .await;
        assert!(matches!(
            response,
            ApiResponse::NotificationsAcknowledged { .. }
        ));
        assert!(env
            .runtime
            .storage
            .pending_notifications()
            .await
            .unwrap()
            .is_empty());

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
                ..ProviderConfig::default()
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
                ..ProviderConfig::default()
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
            ApiResponse::Error { error } => {
                assert_eq!(error.code, ApiErrorCode::InvalidArgument)
            }
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
                ..ProviderConfig::default()
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
    async fn account_rename_is_owned_by_the_database() {
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
                ..ProviderConfig::default()
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

        let persisted = env
            .runtime
            .storage
            .account(&account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.display_name.as_deref(), Some("New name"));
        assert_eq!(
            persisted.display_name_source,
            usage_core::AccountDisplayNameSource::User
        );
        assert!(!env.root.join("config.json").exists());

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
                ..ProviderConfig::default()
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
