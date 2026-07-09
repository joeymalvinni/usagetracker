use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use async_trait::async_trait;
use chrono::{DateTime, Days, Local, NaiveDate, TimeDelta, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};
use wait_timeout::ChildExt;

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{
        DailyUsageBucket, DiscoveredAccount, ProviderCollectionResult, ProviderCollector,
        ProviderError, ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT,
        HTTP_REQUEST_TIMEOUT,
    },
};

pub const PROVIDER_ID: &str = "codex";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_PERCENT: f64 = 100.0;
const COST_LOOKBACK_DAYS: u64 = 30;
const CODEX_APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);
const CODEX_COST_SCAN_MIN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct CodexCollector {
    profiles: Vec<CodexProfile>,
    local_codex_home: PathBuf,
    client: reqwest::Client,
    capture_raw_payloads: bool,
}

#[derive(Clone)]
struct CodexProfile {
    id: String,
    display_name: Option<String>,
    auth_path: PathBuf,
    codex_home: PathBuf,
    cost_cache: Arc<Mutex<Option<CodexCostCache>>>,
}

impl CodexCollector {
    pub fn new(config: ProviderConfig, capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Codex auth"))?;
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent("codex-cli")
            .build()?;
        let local_codex_home = local_codex_home(&home);
        let profiles = codex_profiles(config, &local_codex_home);
        Ok(Self {
            profiles,
            local_codex_home,
            client,
            capture_raw_payloads,
        })
    }

    async fn load_credentials(
        &self,
        profile: &CodexProfile,
    ) -> Result<CodexCredentials, ProviderError> {
        let contents = tokio::fs::read_to_string(&profile.auth_path)
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsMissing,
                        format!("Codex auth file {} is missing", profile.auth_path.display()),
                    )
                } else {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        format!(
                            "failed to read Codex auth file {}",
                            profile.auth_path.display()
                        ),
                    )
                }
            })?;

        codex_credentials_from_auth_json(&contents)
    }

    async fn profile_for_account(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<(CodexProfile, CodexCredentials), ProviderError> {
        if let Some(profile_id) = account.profile_id.as_deref() {
            let profile = self
                .profiles
                .iter()
                .find(|profile| profile.id == profile_id)
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        format!("Codex profile {profile_id} no longer exists"),
                    )
                })?;
            let credentials = self.load_credentials(profile).await?;
            if credentials.account_id != account.external_account_id {
                return Err(ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "Codex account changed since discovery",
                ));
            }
            return Ok((profile.clone(), credentials));
        }

        for profile in &self.profiles {
            let credentials = self.load_credentials(profile).await?;
            if credentials.account_id == account.external_account_id {
                return Ok((profile.clone(), credentials));
            }
        }
        Err(ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Codex account changed since discovery",
        ))
    }

    async fn collect_usage_from_wham(
        &self,
        credentials: &CodexCredentials,
    ) -> Result<CodexCollectedUsage, ProviderError> {
        let started = Instant::now();
        debug!("codex wham usage request started");
        let response = self
            .client
            .get(CODEX_USAGE_URL)
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("Codex usage request failed: {err}"),
                )
            })?;

        let status = response.status();
        debug!(
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            "codex wham usage response received"
        );
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                "Codex credentials were rejected",
            ));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "Codex usage endpoint is rate limited",
            ));
        }
        if !status.is_success() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("Codex usage endpoint returned HTTP {}", status.as_u16()),
            ));
        }

        let payload: Value = response.json().await.map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Codex usage JSON was invalid: {err}"),
            )
        })?;
        let reset_credits = payload.get("rate_limit_reset_credits");
        debug!(
            top_level_keys = ?payload
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>()),
            reset_credits_available = reset_credits
                .and_then(|value| value.get("available_count"))
                .and_then(number_from_json_value),
            reset_credits_has_expiry = reset_credits
                .and_then(|value| value.as_object())
                .is_some_and(|object| {
                    object.contains_key("next_expires_at")
                        || object.contains_key("expires_at")
                        || object.contains_key("credits")
                }),
            "codex wham usage payload parsed"
        );
        let account_display_name = payload
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let usage = normalize_usage(&payload, account_display_name.as_deref())?;

        Ok(CodexCollectedUsage {
            usage,
            daily_usage: Vec::new(),
            account_activity_available: false,
            collection_mode: "wham_usage_api".to_string(),
            account_display_name,
            raw_payload: payload,
            warnings: Vec::new(),
        })
    }
}

fn local_codex_home(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(expand_home_path)
        .unwrap_or_else(|| home.join(".codex"))
}

fn codex_profiles(config: ProviderConfig, local_codex_home: &Path) -> Vec<CodexProfile> {
    let configured = if config.profiles.is_empty() {
        vec![ProviderProfileConfig {
            id: Some("default".to_string()),
            codex_home: Some(local_codex_home.to_path_buf()),
            ..ProviderProfileConfig::default()
        }]
    } else {
        config.profiles
    };

    configured
        .into_iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, profile)| {
            let id = profile_id(profile.id.as_deref(), index);
            let codex_home = profile
                .codex_home
                .map(expand_home_path)
                .unwrap_or_else(|| local_codex_home.to_path_buf());
            let auth_path = profile
                .auth_path
                .map(expand_home_path)
                .unwrap_or_else(|| codex_home.join("auth.json"));
            CodexProfile {
                id,
                display_name: profile.display_name.or_else(|| {
                    if index == 0 {
                        None
                    } else {
                        Some(format!("Codex Account {}", index + 1))
                    }
                }),
                auth_path,
                codex_home,
                cost_cache: Arc::new(Mutex::new(None)),
            }
        })
        .collect()
}

fn profile_id(configured: Option<&str>, index: usize) -> String {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if index == 0 {
                "default".to_string()
            } else {
                format!("profile-{}", index + 1)
            }
        })
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

#[async_trait]
impl ProviderCollector for CodexCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        if self.profiles.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "no enabled Codex profiles are configured",
            ));
        }

        let mut accounts = Vec::new();
        let mut failures = Vec::new();
        for profile in &self.profiles {
            match self.load_credentials(profile).await {
                Ok(credentials) => accounts.push(DiscoveredAccount {
                    external_account_id: credentials.account_id,
                    display_name: profile
                        .display_name
                        .clone()
                        .or(credentials.account_display_name),
                    profile_id: Some(profile.id.clone()),
                }),
                Err(err)
                    if matches!(
                        err.kind(),
                        ProviderErrorKind::CredentialsMissing
                            | ProviderErrorKind::CredentialsInvalid
                    ) =>
                {
                    warn!(
                        profile_id = profile.id.as_str(),
                        error_code = err.kind().as_str(),
                        error = %err,
                        "failed to discover Codex profile"
                    );
                    failures.push(err);
                }
                Err(err) => {
                    failures.push(err);
                }
            }
        }

        if !accounts.is_empty() {
            let mut seen = BTreeSet::new();
            accounts.retain(|account| seen.insert(account.external_account_id.clone()));
            return Ok(accounts);
        }
        Err(failures.into_iter().next().unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "no Codex accounts were discovered",
            )
        }))
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let (profile, credentials) = self.profile_for_account(account).await?;

        let app_server_started = Instant::now();
        info!(
            profile_id = profile.id.as_str(),
            "codex app-server usage collection started"
        );
        let app_server_profile = profile.clone();
        let mut collected = match tokio::task::spawn_blocking(move || {
            collect_usage_from_app_server(&app_server_profile)
        })
        .await
        {
            Ok(Ok(collected)) => {
                info!(
                    elapsed_ms = app_server_started.elapsed().as_millis(),
                    windows = collected.usage.windows.len(),
                    reset_credits_available =
                        reset_credits_available_count(&collected.usage.metadata),
                    reset_credits_next_expires_at =
                        reset_credits_next_expires_at(&collected.usage.metadata),
                    "codex app-server usage collection completed"
                );
                collected
            }
            Ok(Err(app_server_err)) => {
                warn!(
                    elapsed_ms = app_server_started.elapsed().as_millis(),
                    error = %app_server_err,
                    "codex app-server usage failed; falling back to wham"
                );
                let mut fallback = self.collect_usage_from_wham(&credentials).await?;
                fallback.warnings.push(format!(
                    "Codex app-server usage failed; used legacy wham usage API fallback: {}",
                    app_server_err.short_message()
                ));
                fallback
            }
            Err(err) => {
                warn!(
                    elapsed_ms = app_server_started.elapsed().as_millis(),
                    error = %err,
                    "codex app-server usage task failed; falling back to wham"
                );
                let mut fallback = self.collect_usage_from_wham(&credentials).await?;
                fallback.warnings.push(format!(
                    "Codex app-server usage task failed; used legacy wham usage API fallback: {err}"
                ));
                fallback
            }
        };
        collected.usage.metadata["credential_profile"] = json!(profile.id.as_str());
        if let Some(display_name) = profile.display_name.as_deref() {
            collected.usage.metadata["profile_display_name"] = json!(display_name);
        }

        let cost_cache = profile.cost_cache.clone();
        let local_account_id = codex_account_id_from_auth_file(&self.local_codex_home);
        let cost_roots = codex_session_roots(
            &profile.codex_home,
            &self.local_codex_home,
            local_account_id.as_deref(),
            &credentials.account_id,
        );
        let cost_started = Instant::now();
        debug!("codex local cost scan started");
        match tokio::task::spawn_blocking(move || {
            scan_codex_local_costs_cached(cost_cache, cost_roots)
        })
        .await
        {
            Ok(Ok(scan)) => {
                info!(
                    elapsed_ms = cost_started.elapsed().as_millis(),
                    cache_status = scan.cache_status.as_str(),
                    files_scanned = scan.report.files_scanned,
                    token_count_events = scan.report.token_count_events,
                    today_tokens = scan.report.today_tokens,
                    lookback_tokens = scan.report.lookback_tokens,
                    "codex local cost scan completed"
                );
                collected
                    .usage
                    .merge_cost_report(scan.report, !collected.account_activity_available)
            }
            Ok(Err(err)) => collected
                .warnings
                .push(format!("Codex local cost scan failed: {err}")),
            Err(err) => collected
                .warnings
                .push(format!("Codex local cost scan task failed: {err}")),
        }

        Ok(ProviderCollectionResult {
            usage: collected.usage,
            daily_usage: collected.daily_usage,
            collection_mode: collected.collection_mode,
            account_display_name: collected.account_display_name,
            raw_payload: self.capture_raw_payloads.then_some(collected.raw_payload),
            warnings: collected.warnings,
        })
    }
}

#[derive(Debug)]
struct CodexCollectedUsage {
    usage: ProviderUsage,
    daily_usage: Vec<DailyUsageBucket>,
    account_activity_available: bool,
    collection_mode: String,
    account_display_name: Option<String>,
    raw_payload: Value,
    warnings: Vec<String>,
}

fn reset_credits_available_count(metadata: &Value) -> Option<f64> {
    metadata
        .get("rate_limit_reset_credits")
        .and_then(|value| value.get("available_count"))
        .and_then(number_from_json_value)
        .or_else(|| {
            metadata
                .get("rate_limit_reset_credits_available_count")
                .and_then(number_from_json_value)
        })
}

fn reset_credits_next_expires_at(metadata: &Value) -> Option<f64> {
    metadata
        .get("rate_limit_reset_credits")
        .and_then(|value| value.get("next_expires_at"))
        .and_then(number_from_json_value)
}

#[derive(Debug, Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: String,
    account_id: String,
    id_token: Option<String>,
}

#[derive(Debug)]
struct CodexCredentials {
    access_token: String,
    account_id: String,
    account_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexIdTokenClaims {
    email: Option<String>,
    name: Option<String>,
}

fn codex_credentials_from_auth_json(contents: &str) -> Result<CodexCredentials, ProviderError> {
    let auth: CodexAuth = serde_json::from_str(contents).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            format!("Codex auth file is not valid JSON: {err}"),
        )
    })?;

    let tokens = auth.tokens.ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Codex auth file is missing tokens",
        )
    })?;

    if tokens.access_token.trim().is_empty() || tokens.account_id.trim().is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Codex auth file is missing token fields",
        ));
    }

    let account_display_name = tokens
        .id_token
        .as_deref()
        .and_then(codex_display_name_from_id_token);

    Ok(CodexCredentials {
        access_token: tokens.access_token,
        account_id: tokens.account_id,
        account_display_name,
    })
}

fn codex_display_name_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let decoded = decode_base64url(payload)?;
    let claims: CodexIdTokenClaims = serde_json::from_slice(&decoded).ok()?;
    nonempty_string(claims.email).or_else(|| nonempty_string(claims.name))
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity((value.len() * 3).div_ceil(4));
    let mut buffer = 0u32;
    let mut bits = 0u8;

    for byte in value.bytes() {
        let decoded = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;

        buffer = (buffer << 6) | decoded;
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            bytes.push(((buffer >> bits) & 0xff) as u8);
        }
    }

    Some(bytes)
}

fn nonempty_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_usage(
    payload: &Value,
    display_name: Option<&str>,
) -> Result<ProviderUsage, ProviderError> {
    let object = payload.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Codex usage response was not a JSON object",
        )
    })?;

    let mut windows = collect_codex_rate_limit_windows(RateLimitGroupSpec {
        id_prefix: "codex",
        label_prefix: "Codex",
        rate_limit: object.get("rate_limit"),
    });
    windows.extend(collect_codex_rate_limit_windows(RateLimitGroupSpec {
        id_prefix: "codex_code_review",
        label_prefix: "Codex code review",
        rate_limit: object.get("code_review_rate_limit"),
    }));
    windows.extend(collect_additional_rate_limit_windows(
        object.get("additional_rate_limits"),
    ));
    windows.extend(collect_codex_credits_window(object.get("credits")));

    let top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "account_display_name": display_name,
            "email": object.get("email").and_then(Value::as_str),
            "collection_mode": "wham_usage_api",
            "credits_has_credits": object.get("credits").and_then(|value| value.get("has_credits")).and_then(Value::as_bool),
            "credits_overage_limit_reached": object.get("credits").and_then(|value| value.get("overage_limit_reached")).and_then(Value::as_bool),
            "credits_unlimited": object.get("credits").and_then(|value| value.get("unlimited")).and_then(Value::as_bool),
            "plan_type": object.get("plan_type").and_then(Value::as_str),
            "rate_limit_reached_type": object.get("rate_limit_reached_type").and_then(Value::as_str),
            "rate_limit_reset_credits_available_count": object
                .get("rate_limit_reset_credits")
                .and_then(|value| value.get("available_count"))
                .and_then(number_from_json_value),
            "spend_control_reached": object.get("spend_control").and_then(|value| value.get("reached")).and_then(Value::as_bool),
            "top_level_keys": top_level_keys,
        }),
    })
}

fn collect_usage_from_app_server(
    profile: &CodexProfile,
) -> Result<CodexCollectedUsage, ProviderError> {
    let payload = run_codex_app_server_rate_limits(&profile.codex_home).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Codex app-server rate limit request failed: {err}"),
        )
    })?;
    let account_display_name = payload
        .get("account_read")
        .and_then(|value| value.get("account"))
        .and_then(|value| value.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut usage = normalize_app_server_usage(&payload, account_display_name.as_deref())?;
    let mut warnings = Vec::new();
    let (daily_usage, account_activity_available) = match payload
        .get("account_usage_read")
        .filter(|value| !value.is_null())
    {
        Some(value) => match normalize_account_token_usage(value) {
            Ok(activity) => {
                let daily_usage = activity.daily_usage.clone();
                usage.merge_account_activity(activity);
                (daily_usage, true)
            }
            Err(err) => {
                warnings.push(format!(
                    "Codex account activity could not be parsed; using local activity fallback: {}",
                    err.short_message()
                ));
                (Vec::new(), false)
            }
        },
        None => {
            let detail = payload
                .get("account_usage_error")
                .and_then(Value::as_str)
                .unwrap_or("account/usage/read returned no result");
            warnings.push(format!(
                "Codex account activity was unavailable; using local activity fallback: {detail}"
            ));
            (Vec::new(), false)
        }
    };

    Ok(CodexCollectedUsage {
        usage,
        daily_usage,
        account_activity_available,
        collection_mode: "codex_app_server_rate_limits".to_string(),
        account_display_name,
        raw_payload: payload,
        warnings,
    })
}

fn run_codex_app_server_rate_limits(codex_home: &Path) -> anyhow::Result<Value> {
    let started = Instant::now();
    debug!("codex app-server process starting");
    let mut child = Command::new("codex")
        .arg("app-server")
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open codex app-server stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open codex app-server stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open codex app-server stderr"))?;

    let (line_tx, line_rx) = mpsc::channel::<std::io::Result<String>>();
    let _stdout_thread = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let stop = line.is_err();
            if line_tx.send(line).is_err() || stop {
                break;
            }
        }
    });

    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();
    let _stderr_thread = thread::spawn(move || {
        let mut contents = String::new();
        let _ = stderr.read_to_string(&mut contents);
        let _ = stderr_tx.send(contents);
    });

    write_json_rpc(
        &mut stdin,
        &json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "usagetracker",
                    "title": "usagetracker",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "initialized", "params": {} }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({
            "method": "account/read",
            "id": 2,
            "params": { "refreshToken": false }
        }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "account/rateLimits/read", "id": 3 }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "account/usage/read", "id": 4 }),
    )?;

    let deadline = Instant::now() + CODEX_APP_SERVER_TIMEOUT;
    let mut account_read: Option<Value> = None;
    let mut rate_limits_read: Option<Value> = None;
    let mut account_usage_read: Option<Value> = None;
    let mut account_usage_error: Option<String> = None;
    let mut account_usage_complete = false;

    while account_read.is_none() || rate_limits_read.is_none() || !account_usage_complete {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let line = match line_rx.recv_timeout(remaining) {
            Ok(line) => line?,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("codex app-server stdout closed before expected responses");
            }
        };

        let message: Value = serde_json::from_str(&line)?;
        debug!(
            id = message.get("id").and_then(|value| value.as_i64()),
            has_error = message.get("error").is_some(),
            elapsed_ms = started.elapsed().as_millis(),
            "codex app-server message received"
        );
        match message.get("id").and_then(Value::as_i64) {
            Some(2) => account_read = Some(json_rpc_result(message, "account/read")?),
            Some(3) => {
                rate_limits_read = Some(json_rpc_result(message, "account/rateLimits/read")?)
            }
            Some(4) => {
                account_usage_complete = true;
                if let Some(error) = message.get("error") {
                    account_usage_error = Some(error.to_string());
                } else {
                    account_usage_read = message.get("result").cloned();
                    if account_usage_read.is_none() {
                        account_usage_error =
                            Some("account/usage/read response was missing result".to_string());
                    }
                }
            }
            _ => {}
        }
    }

    drop(stdin);
    let _ = child.kill();
    match child.wait_timeout(Duration::from_secs(2))? {
        Some(status) => {
            debug!(
                status = %status,
                elapsed_ms = started.elapsed().as_millis(),
                "codex app-server process exited"
            );
        }
        None => {
            warn!(
                elapsed_ms = started.elapsed().as_millis(),
                "codex app-server process did not exit after kill timeout"
            );
        }
    }
    let stderr = stderr_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap_or_default();

    let account_read = account_read.ok_or_else(|| {
        warn!(
            elapsed_ms = started.elapsed().as_millis(),
            stderr = stderr.trim(),
            "codex app-server account/read timed out"
        );
        anyhow::anyhow!(
            "codex app-server account/read timed out after {:?}; stderr: {}",
            CODEX_APP_SERVER_TIMEOUT,
            stderr.trim()
        )
    })?;
    let rate_limits_read = rate_limits_read.ok_or_else(|| {
        warn!(
            elapsed_ms = started.elapsed().as_millis(),
            stderr = stderr.trim(),
            "codex app-server account/rateLimits/read timed out"
        );
        anyhow::anyhow!(
            "codex app-server account/rateLimits/read timed out after {:?}; stderr: {}",
            CODEX_APP_SERVER_TIMEOUT,
            stderr.trim()
        )
    })?;
    if !account_usage_complete {
        account_usage_error = Some(format!(
            "account/usage/read timed out after {:?}",
            CODEX_APP_SERVER_TIMEOUT
        ));
    }

    debug!(
        elapsed_ms = started.elapsed().as_millis(),
        "codex app-server process completed"
    );

    Ok(json!({
        "account_read": account_read,
        "rate_limits_read": rate_limits_read,
        "account_usage_read": account_usage_read,
        "account_usage_error": account_usage_error,
    }))
}

fn write_json_rpc(stdin: &mut impl Write, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stdin, message)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn json_rpc_result(message: Value, method: &str) -> anyhow::Result<Value> {
    if let Some(error) = message.get("error") {
        anyhow::bail!("{method} returned error: {error}");
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{method} response missing result"))
}

fn normalize_app_server_usage(
    payload: &Value,
    display_name: Option<&str>,
) -> Result<ProviderUsage, ProviderError> {
    let rate_limits_read = payload
        .get("rate_limits_read")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Codex app-server rate limits response was not a JSON object",
            )
        })?;
    let account = payload
        .get("account_read")
        .and_then(|value| value.get("account"));
    let main_rate_limit = rate_limits_read
        .get("rateLimitsByLimitId")
        .and_then(|value| value.get("codex"))
        .or_else(|| rate_limits_read.get("rateLimits"));

    let mut windows = collect_app_server_rate_limit_windows(AppServerRateLimitGroupSpec {
        id_prefix: "codex",
        label_prefix: "Codex",
        rate_limit: main_rate_limit,
    });
    windows.extend(collect_app_server_additional_rate_limit_windows(
        rate_limits_read.get("rateLimitsByLimitId"),
    ));
    windows.extend(collect_app_server_credits_window(
        main_rate_limit.and_then(|value| value.get("credits")),
    ));

    let reset_credits = rate_limits_read.get("rateLimitResetCredits");
    let top_level_keys = rate_limits_read.keys().cloned().collect::<Vec<_>>();
    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "account_display_name": display_name,
            "email": account.and_then(|value| value.get("email")).and_then(Value::as_str),
            "collection_mode": "codex_app_server_rate_limits",
            "credits_has_credits": main_rate_limit
                .and_then(|value| value.get("credits"))
                .and_then(|value| value.get("hasCredits"))
                .and_then(Value::as_bool),
            "credits_overage_limit_reached": Value::Null,
            "credits_unlimited": main_rate_limit
                .and_then(|value| value.get("credits"))
                .and_then(|value| value.get("unlimited"))
                .and_then(Value::as_bool),
            "plan_type": account
                .and_then(|value| value.get("planType"))
                .and_then(Value::as_str)
                .or_else(|| main_rate_limit.and_then(|value| value.get("planType")).and_then(Value::as_str)),
            "rate_limit_reached_type": main_rate_limit
                .and_then(|value| value.get("rateLimitReachedType"))
                .and_then(Value::as_str),
            "rate_limit_reset_credits_available_count": reset_credits
                .and_then(|value| value.get("availableCount"))
                .and_then(number_from_json_value),
            "rate_limit_reset_credits": app_server_reset_credits_metadata(reset_credits),
            "spend_control_reached": Value::Null,
            "top_level_keys": top_level_keys,
        }),
    })
}

#[derive(Clone, Debug)]
struct CodexAccountActivity {
    daily_usage: Vec<DailyUsageBucket>,
    lifetime_tokens: Option<u64>,
    peak_daily_tokens: Option<u64>,
    longest_running_turn_sec: Option<u64>,
    current_streak_days: Option<u64>,
    longest_streak_days: Option<u64>,
}

fn normalize_account_token_usage(value: &Value) -> Result<CodexAccountActivity, ProviderError> {
    let object = value.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Codex account usage response was not an object",
        )
    })?;
    let summary = object
        .get("summary")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Codex account usage response was missing summary",
            )
        })?;

    let mut by_date = BTreeMap::<NaiveDate, u64>::new();
    if let Some(buckets) = object
        .get("dailyUsageBuckets")
        .filter(|value| !value.is_null())
    {
        let buckets = buckets.as_array().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Codex account usage daily buckets were not an array",
            )
        })?;
        for bucket in buckets {
            let bucket = bucket.as_object().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Parse,
                    "Codex account usage contained a non-object daily bucket",
                )
            })?;
            let date = bucket
                .get("startDate")
                .and_then(Value::as_str)
                .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Parse,
                        "Codex account usage daily bucket had an invalid startDate",
                    )
                })?;
            let tokens = bucket
                .get("tokens")
                .and_then(u64_from_json_value)
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Parse,
                        "Codex account usage daily bucket had invalid tokens",
                    )
                })?;
            let entry = by_date.entry(date).or_default();
            *entry = entry.saturating_add(tokens);
        }
    }

    Ok(CodexAccountActivity {
        daily_usage: by_date
            .into_iter()
            .map(|(date, tokens)| DailyUsageBucket {
                date,
                tokens,
                cost_usd: None,
                source: "codex_account_usage".to_string(),
            })
            .collect(),
        lifetime_tokens: summary.get("lifetimeTokens").and_then(u64_from_json_value),
        peak_daily_tokens: summary.get("peakDailyTokens").and_then(u64_from_json_value),
        longest_running_turn_sec: summary
            .get("longestRunningTurnSec")
            .and_then(u64_from_json_value),
        current_streak_days: summary
            .get("currentStreakDays")
            .and_then(u64_from_json_value),
        longest_streak_days: summary
            .get("longestStreakDays")
            .and_then(u64_from_json_value),
    })
}

trait CodexAccountActivityExt {
    fn merge_account_activity(&mut self, activity: CodexAccountActivity);
}

impl CodexAccountActivityExt for ProviderUsage {
    fn merge_account_activity(&mut self, activity: CodexAccountActivity) {
        let today = Local::now().date_naive();
        let lookback_start = today
            .checked_sub_days(Days::new(COST_LOOKBACK_DAYS.saturating_sub(1)))
            .unwrap_or(today);
        let today_tokens = activity
            .daily_usage
            .iter()
            .find(|bucket| bucket.date == today)
            .map(|bucket| bucket.tokens)
            .unwrap_or(0);
        let lookback_tokens = activity
            .daily_usage
            .iter()
            .filter(|bucket| bucket.date >= lookback_start && bucket.date <= today)
            .fold(0_u64, |total, bucket| total.saturating_add(bucket.tokens));
        let bucket_sum = activity
            .daily_usage
            .iter()
            .fold(0_u64, |total, bucket| total.saturating_add(bucket.tokens));
        let lifetime_tokens = activity.lifetime_tokens.unwrap_or(bucket_sum);

        if today_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_today",
                "Codex tokens today",
                today_tokens,
                UsageWindowKind::Daily,
            ));
        }
        if lookback_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_30d",
                "Codex tokens 30 days",
                lookback_tokens,
                UsageWindowKind::Monthly,
            ));
        }
        if lifetime_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_lifetime",
                "Codex lifetime tokens",
                lifetime_tokens,
                UsageWindowKind::Other("lifetime".to_string()),
            ));
        }

        let by_day = activity
            .daily_usage
            .iter()
            .map(|bucket| {
                json!({
                    "date": bucket.date.to_string(),
                    "tokens": bucket.tokens,
                })
            })
            .collect::<Vec<_>>();
        self.metadata["codex_activity"] = json!({
            "source": "codex_account_usage",
            "server_authoritative": true,
            "daily_bucket_count": activity.daily_usage.len(),
            "today_tokens": today_tokens,
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_tokens": lookback_tokens,
            "lifetime_tokens": lifetime_tokens,
            "peak_daily_tokens": activity.peak_daily_tokens,
            "longest_running_turn_sec": activity.longest_running_turn_sec,
            "current_streak_days": activity.current_streak_days,
            "longest_streak_days": activity.longest_streak_days,
            "by_day": by_day,
        });
    }
}

trait CodexUsageCostExt {
    fn merge_cost_report(&mut self, report: CodexCostReport, include_token_activity: bool);
}

impl CodexUsageCostExt for ProviderUsage {
    fn merge_cost_report(&mut self, report: CodexCostReport, include_token_activity: bool) {
        if report.total_tokens == 0 {
            self.metadata["codex_cost"] = json!({
                "source": "local_session_logs",
                "estimate": true,
                "partial": true,
                "complete_lookback": false,
                "session_roots": report.session_roots,
                "files_scanned": report.files_scanned,
                "token_count_events": report.token_count_events,
                "unpriced_tokens": report.unpriced_tokens,
            });
            return;
        }

        if report.today_tokens > 0 {
            self.windows.push(cost_window(
                "codex_estimated_spend_today",
                "Codex spend today",
                report.today_cost_usd,
            ));
            if include_token_activity {
                self.windows.push(token_window(
                    "codex_tokens_today",
                    "Codex tokens today",
                    report.today_tokens,
                    UsageWindowKind::Daily,
                ));
            }
        }

        if report.lookback_tokens > 0 {
            self.windows.push(cost_window(
                "codex_estimated_spend_30d",
                "Codex spend 30 days",
                report.lookback_cost_usd,
            ));
            if include_token_activity {
                self.windows.push(token_window(
                    "codex_tokens_30d",
                    "Codex tokens 30 days",
                    report.lookback_tokens,
                    UsageWindowKind::Monthly,
                ));
            }
        }

        self.metadata["codex_cost"] = json!({
            "source": "local_session_logs",
            "estimate": true,
            "partial": true,
            "complete_lookback": false,
            "hint": "Estimated from this device's local Codex logs; account-wide token activity is tracked separately.",
            "session_roots": report.session_roots,
            "files_scanned": report.files_scanned,
            "token_count_events": report.token_count_events,
            "today_cost_usd": report.today_cost_usd,
            "today_tokens": report.today_tokens,
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_cost_usd": report.lookback_cost_usd,
            "lookback_tokens": report.lookback_tokens,
            "total_cost_usd": report.total_cost_usd,
            "total_tokens": report.total_tokens,
            "unpriced_tokens": report.unpriced_tokens,
            "by_day": daily_cost_rows(&report.by_day),
            "by_model": report.by_model,
        });
    }
}

fn cost_window(window_id: &str, label: &str, value: f64) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Credits,
        used: Some(UsageAmount {
            value,
            unit: UsageUnit::Usd,
        }),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

fn token_window(window_id: &str, label: &str, tokens: u64, kind: UsageWindowKind) -> UsageWindow {
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

#[derive(Clone, Debug)]
struct CodexCostCache {
    fingerprint: CodexSessionFingerprint,
    report: CodexCostReport,
    scanned_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexCostCacheStatus {
    Hit,
    Throttled,
    Refreshed,
}

impl CodexCostCacheStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Throttled => "throttled",
            Self::Refreshed => "refreshed",
        }
    }
}

#[derive(Clone, Debug)]
struct CodexCostScan {
    report: CodexCostReport,
    cache_status: CodexCostCacheStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CodexSessionFingerprint {
    files: usize,
    total_size: u64,
    latest_modified_ns: u128,
}

#[derive(Clone, Debug, Default)]
struct CodexCostReport {
    session_roots: Vec<String>,
    files_scanned: usize,
    token_count_events: usize,
    today_cost_usd: f64,
    today_tokens: u64,
    lookback_cost_usd: f64,
    lookback_tokens: u64,
    total_cost_usd: f64,
    total_tokens: u64,
    unpriced_tokens: u64,
    by_day: BTreeMap<NaiveDate, DailyCostSummary>,
    by_model: BTreeMap<String, CodexModelCostSummary>,
}

#[derive(Clone, Debug, Default)]
struct DailyCostSummary {
    cost_usd: f64,
    tokens: u64,
}

#[derive(Debug, serde::Serialize)]
struct DailyCostRow {
    date: String,
    cost_usd: f64,
    tokens: u64,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct CodexModelCostSummary {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct CodexTokenTotals {
    input: u64,
    cached: u64,
    output: u64,
}

impl CodexTokenTotals {
    fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }

    fn saturating_delta(self, previous: Self) -> Self {
        Self {
            input: self.input.saturating_sub(previous.input),
            cached: self.cached.saturating_sub(previous.cached),
            output: self.output.saturating_sub(previous.output),
        }
    }
}

fn scan_codex_local_costs_cached(
    cache: Arc<Mutex<Option<CodexCostCache>>>,
    roots: Vec<PathBuf>,
) -> anyhow::Result<CodexCostScan> {
    let fingerprint = codex_session_fingerprint(&roots)?;
    let session_roots = roots
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if let Some(cached) = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Codex cost cache mutex poisoned"))?
        .as_ref()
    {
        let same_roots = cached.report.session_roots == session_roots;
        if same_roots && cached.fingerprint == fingerprint {
            return Ok(CodexCostScan {
                report: cached.report.clone(),
                cache_status: CodexCostCacheStatus::Hit,
            });
        }
        if same_roots && cached.scanned_at.elapsed() < CODEX_COST_SCAN_MIN_INTERVAL {
            return Ok(CodexCostScan {
                report: cached.report.clone(),
                cache_status: CodexCostCacheStatus::Throttled,
            });
        }
    }

    let report = scan_codex_local_costs_from_roots(roots)?;
    *cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Codex cost cache mutex poisoned"))? = Some(CodexCostCache {
        fingerprint,
        report: report.clone(),
        scanned_at: Instant::now(),
    });
    Ok(CodexCostScan {
        report,
        cache_status: CodexCostCacheStatus::Refreshed,
    })
}

fn scan_codex_local_costs_from_roots(roots: Vec<PathBuf>) -> anyhow::Result<CodexCostReport> {
    let today = Local::now().date_naive();
    let lookback_start = today
        .checked_sub_days(Days::new(COST_LOOKBACK_DAYS.saturating_sub(1)))
        .unwrap_or(today);
    let mut report = CodexCostReport {
        session_roots: roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        ..Default::default()
    };

    for root in roots {
        collect_codex_session_files(&root, &mut |path| {
            report.files_scanned += 1;
            scan_codex_session_file(path, today, lookback_start, &mut report)
        })?;
    }

    Ok(report)
}

fn codex_session_fingerprint(roots: &[PathBuf]) -> anyhow::Result<CodexSessionFingerprint> {
    let mut fingerprint = CodexSessionFingerprint::default();
    for root in roots {
        collect_codex_session_files(root, &mut |path| {
            let metadata = std::fs::metadata(path)?;
            fingerprint.files += 1;
            fingerprint.total_size = fingerprint.total_size.saturating_add(metadata.len());
            let modified_ns = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            fingerprint.latest_modified_ns = fingerprint.latest_modified_ns.max(modified_ns);
            Ok(())
        })?;
    }
    Ok(fingerprint)
}

fn codex_account_id_from_auth_file(codex_home: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(codex_home.join("auth.json")).ok()?;
    let auth: CodexAuth = serde_json::from_str(&contents).ok()?;
    nonempty_string(Some(auth.tokens?.account_id))
}

fn codex_session_roots(
    profile_home: &Path,
    local_codex_home: &Path,
    local_account_id: Option<&str>,
    profile_account_id: &str,
) -> Vec<PathBuf> {
    let mut roots = vec![profile_home.join("sessions")];
    if profile_home != local_codex_home && local_account_id == Some(profile_account_id) {
        roots.push(local_codex_home.join("sessions"));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn collect_codex_session_files(
    path: &Path,
    visit: &mut impl FnMut(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_codex_session_files(&path, visit)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            visit(&path)?;
        }
    }
    Ok(())
}

fn scan_codex_session_file(
    path: &Path,
    today: NaiveDate,
    lookback_start: NaiveDate,
    report: &mut CodexCostReport,
) -> anyhow::Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut current_model: Option<String> = None;
    let mut previous_totals: Option<CodexTokenTotals> = None;

    for line in reader.lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if let Some(model) = codex_turn_context_model(&event) {
            current_model = Some(model.to_string());
        }

        let Some(info) = codex_token_count_info(&event) else {
            continue;
        };

        report.token_count_events += 1;
        let delta = info
            .get("last_token_usage")
            .and_then(codex_totals_from_value)
            .or_else(|| {
                let current = info
                    .get("total_token_usage")
                    .and_then(codex_totals_from_value)?;
                let previous = previous_totals.unwrap_or_default();
                Some(current.saturating_delta(previous))
            })
            .unwrap_or_default();

        if let Some(total) = info
            .get("total_token_usage")
            .and_then(codex_totals_from_value)
        {
            previous_totals = Some(total);
        } else {
            previous_totals = Some(previous_totals.unwrap_or_default().add(delta));
        }

        if delta.total() == 0 {
            continue;
        }

        let model = current_model.as_deref().unwrap_or("unknown");
        let cost = codex_cost_usd(model, delta);
        let tokens = delta.total();
        let date = codex_event_timestamp(&event)
            .map(|timestamp| timestamp.with_timezone(&Local).date_naive())
            .unwrap_or(today);

        report.total_tokens = report.total_tokens.saturating_add(tokens);
        if let Some(cost) = cost {
            report.total_cost_usd += cost;
        } else {
            report.unpriced_tokens = report.unpriced_tokens.saturating_add(tokens);
        }

        if date == today {
            report.today_tokens = report.today_tokens.saturating_add(tokens);
            if let Some(cost) = cost {
                report.today_cost_usd += cost;
            }
        }
        if date >= lookback_start && date <= today {
            report.lookback_tokens = report.lookback_tokens.saturating_add(tokens);
            if let Some(cost) = cost {
                report.lookback_cost_usd += cost;
            }
            let day = report.by_day.entry(date).or_default();
            day.tokens = day.tokens.saturating_add(tokens);
            if let Some(cost) = cost {
                day.cost_usd += cost;
            }
        }

        let summary = report
            .by_model
            .entry(normalize_codex_model(model))
            .or_default();
        summary.input_tokens = summary.input_tokens.saturating_add(delta.input);
        summary.cached_input_tokens = summary.cached_input_tokens.saturating_add(delta.cached);
        summary.output_tokens = summary.output_tokens.saturating_add(delta.output);
        if let Some(cost) = cost {
            summary.cost_usd += cost;
        }
    }

    Ok(())
}

fn daily_cost_rows(by_day: &BTreeMap<NaiveDate, DailyCostSummary>) -> Vec<DailyCostRow> {
    by_day
        .iter()
        .map(|(date, summary)| DailyCostRow {
            date: date.to_string(),
            cost_usd: summary.cost_usd,
            tokens: summary.tokens,
        })
        .collect()
}

trait CodexTotalsAdd {
    fn add(self, delta: CodexTokenTotals) -> Self;
}

impl CodexTotalsAdd for CodexTokenTotals {
    fn add(self, delta: CodexTokenTotals) -> Self {
        Self {
            input: self.input.saturating_add(delta.input),
            cached: self.cached.saturating_add(delta.cached),
            output: self.output.saturating_add(delta.output),
        }
    }
}

fn codex_token_count_info(event: &Value) -> Option<&Value> {
    if event.get("type").and_then(Value::as_str) == Some("token_count") {
        return event
            .get("info")
            .or_else(|| event.get("payload")?.get("info"));
    }

    let payload = event.get("payload")?;
    if payload.get("type").and_then(Value::as_str) == Some("token_count") {
        return payload.get("info");
    }
    None
}

fn codex_turn_context_model(event: &Value) -> Option<&str> {
    if event.get("type").and_then(Value::as_str) == Some("turn_context") {
        return event
            .get("payload")
            .and_then(|payload| payload.get("model"))
            .and_then(Value::as_str);
    }

    let payload = event.get("payload")?;
    if payload.get("type").and_then(Value::as_str) == Some("turn_context") {
        return payload.get("payload")?.get("model").and_then(Value::as_str);
    }
    None
}

fn codex_event_timestamp(event: &Value) -> Option<DateTime<Utc>> {
    let timestamp = event.get("timestamp").and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn codex_totals_from_value(value: &Value) -> Option<CodexTokenTotals> {
    Some(CodexTokenTotals {
        input: u64_from_json_value(value.get("input_tokens")?)?,
        cached: value
            .get("cached_input_tokens")
            .and_then(u64_from_json_value)
            .unwrap_or(0),
        output: u64_from_json_value(value.get("output_tokens")?)?,
    })
}

fn u64_from_json_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct CodexPricing {
    input: f64,
    cached_input: Option<f64>,
    output: f64,
    long_context_threshold: Option<u64>,
    long_context_multiplier: f64,
}

fn codex_cost_usd(model: &str, totals: CodexTokenTotals) -> Option<f64> {
    let pricing = codex_pricing(model)?;
    let cached = totals.cached.min(totals.input);
    let non_cached = totals.input.saturating_sub(cached);
    let multiplier = pricing
        .long_context_threshold
        .filter(|threshold| totals.input > *threshold)
        .map(|_| pricing.long_context_multiplier)
        .unwrap_or(1.0);
    let cached_rate = pricing.cached_input.unwrap_or(pricing.input);

    Some(
        non_cached as f64 * pricing.input * multiplier
            + cached as f64 * cached_rate * multiplier
            + totals.output as f64 * pricing.output * multiplier,
    )
}

fn codex_pricing(model: &str) -> Option<CodexPricing> {
    let model = normalize_codex_model(model);
    let p = |input_per_million: f64, output_per_million: f64, cache_per_million: Option<f64>| {
        CodexPricing {
            input: input_per_million / 1_000_000.0,
            cached_input: cache_per_million.map(|value| value / 1_000_000.0),
            output: output_per_million / 1_000_000.0,
            long_context_threshold: None,
            long_context_multiplier: 1.0,
        }
    };
    let lc =
        |input_per_million: f64, output_per_million: f64, cache_per_million: f64| CodexPricing {
            input: input_per_million / 1_000_000.0,
            cached_input: Some(cache_per_million / 1_000_000.0),
            output: output_per_million / 1_000_000.0,
            long_context_threshold: Some(272_000),
            long_context_multiplier: 2.0,
        };

    Some(match model.as_str() {
        "gpt-5" | "gpt-5-codex" | "gpt-5.1" | "gpt-5.1-codex" | "gpt-5.1-codex-max" => {
            p(1.25, 10.00, Some(0.125))
        }
        "gpt-5-mini" => p(0.25, 2.00, Some(0.025)),
        "gpt-5-nano" => p(0.05, 0.40, Some(0.005)),
        "gpt-5-pro" => p(15.00, 120.00, None),
        "gpt-5.2" | "gpt-5.2-codex" | "gpt-5.3-codex" => p(1.75, 14.00, Some(0.175)),
        "gpt-5.2-pro" => p(21.00, 168.00, None),
        "gpt-5.3-codex-spark" => p(0.0, 0.0, Some(0.0)),
        "gpt-5.4" => lc(2.50, 15.00, 0.25),
        "gpt-5.4-mini" => p(0.75, 4.50, Some(0.075)),
        "gpt-5.4-nano" => p(0.20, 1.25, Some(0.02)),
        "gpt-5.4-pro" | "gpt-5.5-pro" => p(30.00, 180.00, None),
        "gpt-5.5" => lc(5.00, 30.00, 0.50),
        _ => return None,
    })
}

fn normalize_codex_model(model: &str) -> String {
    let model = model.strip_prefix("openai/").unwrap_or(model).trim();
    if model.len() > 11 && model.as_bytes()[model.len() - 11] == b'-' {
        let suffix = &model[model.len() - 10..];
        if suffix.len() == 10
            && suffix.as_bytes()[4] == b'-'
            && suffix.as_bytes()[7] == b'-'
            && suffix
                .as_bytes()
                .iter()
                .enumerate()
                .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        {
            return model[..model.len() - 11].to_string();
        }
    }
    model.to_string()
}

struct RateLimitGroupSpec<'a> {
    id_prefix: &'a str,
    label_prefix: &'a str,
    rate_limit: Option<&'a Value>,
}

struct RateLimitWindowSpec<'a> {
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    value: Option<&'a Value>,
}

struct AppServerRateLimitGroupSpec<'a> {
    id_prefix: &'a str,
    label_prefix: &'a str,
    rate_limit: Option<&'a Value>,
}

struct AppServerRateLimitWindowSpec<'a> {
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    value: Option<&'a Value>,
}

fn collect_codex_rate_limit_windows(spec: RateLimitGroupSpec<'_>) -> Vec<UsageWindow> {
    let Some(rate_limit) = spec.rate_limit.and_then(Value::as_object) else {
        return Vec::new();
    };

    [
        rate_limit_window(RateLimitWindowSpec {
            window_id: format!("{}_session", spec.id_prefix),
            label: format!("{} session", spec.label_prefix),
            kind: UsageWindowKind::Session,
            value: rate_limit.get("primary_window"),
        }),
        rate_limit_window(RateLimitWindowSpec {
            window_id: format!("{}_weekly", spec.id_prefix),
            label: format!("{} weekly", spec.label_prefix),
            kind: UsageWindowKind::Weekly,
            value: rate_limit.get("secondary_window"),
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn collect_app_server_rate_limit_windows(
    spec: AppServerRateLimitGroupSpec<'_>,
) -> Vec<UsageWindow> {
    let Some(rate_limit) = spec.rate_limit.and_then(Value::as_object) else {
        return Vec::new();
    };

    [
        app_server_rate_limit_window(AppServerRateLimitWindowSpec {
            window_id: format!("{}_session", spec.id_prefix),
            label: format!("{} session", spec.label_prefix),
            kind: UsageWindowKind::Session,
            value: rate_limit.get("primary"),
        }),
        app_server_rate_limit_window(AppServerRateLimitWindowSpec {
            window_id: format!("{}_weekly", spec.id_prefix),
            label: format!("{} weekly", spec.label_prefix),
            kind: UsageWindowKind::Weekly,
            value: rate_limit.get("secondary"),
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn collect_additional_rate_limit_windows(value: Option<&Value>) -> Vec<UsageWindow> {
    let Some(rate_limits) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    rate_limits
        .iter()
        .enumerate()
        .filter_map(|(index, rate_limit)| {
            let rate_limit = rate_limit.as_object()?;
            let label = rate_limit
                .get("limit_name")
                .and_then(Value::as_str)
                .or_else(|| rate_limit.get("metered_feature").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Codex additional limit {}", index + 1));
            let id_prefix = format!("codex_additional_{index}");
            Some(collect_codex_rate_limit_windows(RateLimitGroupSpec {
                id_prefix: &id_prefix,
                label_prefix: &label,
                rate_limit: rate_limit.get("rate_limit"),
            }))
        })
        .flatten()
        .collect()
}

fn collect_app_server_additional_rate_limit_windows(value: Option<&Value>) -> Vec<UsageWindow> {
    let Some(rate_limits) = value.and_then(Value::as_object) else {
        return Vec::new();
    };

    rate_limits
        .iter()
        .filter(|(limit_id, _)| limit_id.as_str() != "codex")
        .enumerate()
        .flat_map(|(index, (limit_id, rate_limit))| {
            let label = rate_limit
                .get("limitName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(limit_id);
            let id_prefix = format!("codex_additional_{index}");
            collect_app_server_rate_limit_windows(AppServerRateLimitGroupSpec {
                id_prefix: &id_prefix,
                label_prefix: label,
                rate_limit: Some(rate_limit),
            })
        })
        .collect()
}

fn collect_codex_credits_window(credits: Option<&Value>) -> Option<UsageWindow> {
    let credits = credits.and_then(Value::as_object)?;

    let balance = credits.get("balance").and_then(number_from_json_value);
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if balance.is_none() && !unlimited {
        return None;
    }

    let remaining = if unlimited { None } else { balance };
    let mut metadata_label = "Codex credits".to_string();
    if unlimited {
        metadata_label.push_str(" (unlimited)");
    }

    Some(UsageWindow {
        window_id: "codex_credits".to_string(),
        label: metadata_label,
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: remaining.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    })
}

fn collect_app_server_credits_window(credits: Option<&Value>) -> Option<UsageWindow> {
    let credits = credits.and_then(Value::as_object)?;

    let balance = credits.get("balance").and_then(number_from_json_value);
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if balance.is_none() && !unlimited {
        return None;
    }

    let remaining = if unlimited { None } else { balance };
    let mut metadata_label = "Codex credits".to_string();
    if unlimited {
        metadata_label.push_str(" (unlimited)");
    }

    Some(UsageWindow {
        window_id: "codex_credits".to_string(),
        label: metadata_label,
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: remaining.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    })
}

fn rate_limit_window(spec: RateLimitWindowSpec<'_>) -> Option<UsageWindow> {
    let object = spec.value?.as_object()?;
    let percent_used = object
        .get("used_percent")
        .and_then(number_from_json_value)
        .map(|value| value.clamp(0.0, MAX_PERCENT));
    let percent_remaining = percent_used.map(|value| MAX_PERCENT - value);
    let reset_at = object
        .get("reset_at")
        .and_then(unix_timestamp_from_json_value)
        .or_else(|| {
            object
                .get("reset_after_seconds")
                .and_then(number_from_json_value)
                .and_then(|seconds| TimeDelta::try_seconds(seconds.round() as i64))
                .map(|duration| Utc::now() + duration)
        });

    if percent_used.is_none() && reset_at.is_none() {
        return None;
    }

    Some(UsageWindow {
        window_id: spec.window_id,
        label: spec.label,
        kind: spec.kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used,
        percent_remaining,
        reset_at,
    })
}

fn app_server_rate_limit_window(spec: AppServerRateLimitWindowSpec<'_>) -> Option<UsageWindow> {
    let object = spec.value?.as_object()?;
    let percent_used = object
        .get("usedPercent")
        .and_then(number_from_json_value)
        .map(|value| value.clamp(0.0, MAX_PERCENT));
    let percent_remaining = percent_used.map(|value| MAX_PERCENT - value);
    let reset_at = object
        .get("resetsAt")
        .and_then(unix_timestamp_from_json_value);

    if percent_used.is_none() && reset_at.is_none() {
        return None;
    }

    Some(UsageWindow {
        window_id: spec.window_id,
        label: spec.label,
        kind: spec.kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used,
        percent_remaining,
        reset_at,
    })
}

fn app_server_reset_credits_metadata(reset_credits: Option<&Value>) -> Value {
    let Some(reset_credits) = reset_credits.and_then(Value::as_object) else {
        return Value::Null;
    };

    let credits = reset_credits
        .get("credits")
        .and_then(Value::as_array)
        .map(|credits| {
            credits
                .iter()
                .filter_map(app_server_reset_credit_metadata)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let next_expires_at = credits
        .iter()
        .filter(|credit| {
            credit
                .get("status")
                .and_then(Value::as_str)
                .is_none_or(|status| status == "available")
        })
        .filter_map(|credit| credit.get("expires_at").and_then(number_from_json_value))
        .min_by(|left, right| left.total_cmp(right));
    let next_expires_at_iso = next_expires_at.and_then(unix_seconds_iso);

    json!({
        "available_count": reset_credits
            .get("availableCount")
            .and_then(number_from_json_value),
        "credits": credits,
        "next_expires_at": next_expires_at,
        "next_expires_at_iso": next_expires_at_iso,
    })
}

fn app_server_reset_credit_metadata(credit: &Value) -> Option<Value> {
    let credit = credit.as_object()?;
    let granted_at = credit.get("grantedAt").and_then(number_from_json_value);
    let expires_at = credit.get("expiresAt").and_then(number_from_json_value);
    Some(json!({
        "id": credit.get("id").and_then(Value::as_str),
        "status": credit.get("status").and_then(Value::as_str),
        "reset_type": credit.get("resetType").and_then(Value::as_str),
        "granted_at": granted_at,
        "granted_at_iso": granted_at.and_then(unix_seconds_iso),
        "expires_at": expires_at,
        "expires_at_iso": expires_at.and_then(unix_seconds_iso),
        "title": credit.get("title").and_then(Value::as_str),
        "description": credit.get("description").and_then(Value::as_str),
    }))
}

fn unix_seconds_iso(seconds: f64) -> Option<String> {
    DateTime::from_timestamp(seconds.round() as i64, 0).map(|time| time.to_rfc3339())
}

fn number_from_json_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn unix_timestamp_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    let seconds = number_from_json_value(value)?.round() as i64;
    DateTime::from_timestamp(seconds, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::AccountId;

    #[test]
    fn adds_standard_codex_sessions_only_for_the_active_account() {
        let profile_home = Path::new("/profiles/personal");
        let local_home = Path::new("/home/.codex");

        let matching = codex_session_roots(
            profile_home,
            local_home,
            Some("personal-account"),
            "personal-account",
        );
        assert_eq!(matching.len(), 2);
        assert!(matching.contains(&profile_home.join("sessions")));
        assert!(matching.contains(&local_home.join("sessions")));

        let different = codex_session_roots(
            Path::new("/profiles/work"),
            local_home,
            Some("personal-account"),
            "work-account",
        );
        assert_eq!(different, vec![PathBuf::from("/profiles/work/sessions")]);
    }

    #[test]
    fn does_not_duplicate_standard_codex_session_root() {
        let local_home = Path::new("/home/.codex");
        let roots = codex_session_roots(
            local_home,
            local_home,
            Some("personal-account"),
            "personal-account",
        );

        assert_eq!(roots, vec![local_home.join("sessions")]);
    }

    #[test]
    fn account_activity_is_authoritative_and_local_cost_does_not_duplicate_tokens() {
        let today = Local::now().date_naive();
        let yesterday = today.checked_sub_days(Days::new(1)).unwrap();
        let activity = normalize_account_token_usage(&json!({
            "summary": {
                "lifetimeTokens": 300,
                "peakDailyTokens": 200,
                "longestRunningTurnSec": 90,
                "currentStreakDays": 2,
                "longestStreakDays": 4
            },
            "dailyUsageBuckets": [
                {"startDate": yesterday.to_string(), "tokens": 200},
                {"startDate": today.to_string(), "tokens": 100}
            ]
        }))
        .unwrap();
        assert_eq!(activity.daily_usage.len(), 2);
        assert_eq!(activity.daily_usage[0].date, yesterday);
        assert_eq!(activity.daily_usage[1].tokens, 100);

        let mut usage = ProviderUsage {
            provider_id: ProviderId::new(PROVIDER_ID),
            collected_at: Utc::now(),
            windows: Vec::new(),
            metadata: json!({}),
        };
        usage.merge_account_activity(activity);

        let mut by_day = BTreeMap::new();
        by_day.insert(
            today,
            DailyCostSummary {
                cost_usd: 1.25,
                tokens: 100,
            },
        );
        usage.merge_cost_report(
            CodexCostReport {
                today_cost_usd: 1.25,
                today_tokens: 100,
                lookback_cost_usd: 1.25,
                lookback_tokens: 100,
                total_cost_usd: 1.25,
                total_tokens: 100,
                by_day,
                ..Default::default()
            },
            false,
        );

        assert_eq!(usage.metadata["codex_activity"]["lifetime_tokens"], 300);
        assert_eq!(usage.metadata["codex_activity"]["by_day"][1]["tokens"], 100);
        assert_eq!(usage.metadata["codex_cost"]["partial"], true);
        assert_eq!(
            usage
                .windows
                .iter()
                .filter(|window| window.window_id == "codex_tokens_today")
                .count(),
            1
        );
        assert!(usage
            .windows
            .iter()
            .any(|window| window.window_id == "codex_estimated_spend_today"));
    }

    #[test]
    fn reads_codex_identity_from_id_token_claims() {
        let credentials = codex_credentials_from_auth_json(
            r#"{
                "tokens": {
                    "access_token": "access",
                    "account_id": "account-id",
                    "id_token": "header.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJuYW1lIjoiRXhhbXBsZSBVc2VyIn0.signature"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(credentials.access_token, "access");
        assert_eq!(credentials.account_id, "account-id");
        assert_eq!(
            credentials.account_display_name.as_deref(),
            Some("user@example.com")
        );
    }

    #[test]
    fn falls_back_to_codex_name_when_id_token_email_is_blank() {
        let credentials = codex_credentials_from_auth_json(
            r#"{
                "tokens": {
                    "access_token": "access",
                    "account_id": "account-id",
                    "id_token": "header.eyJlbWFpbCI6IiAgICIsIm5hbWUiOiJFeGFtcGxlIFVzZXIifQ.signature"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            credentials.account_display_name.as_deref(),
            Some("Example User")
        );
    }

    #[test]
    fn ignores_invalid_codex_id_token_identity_claims() {
        let credentials = codex_credentials_from_auth_json(
            r#"{
                "tokens": {
                    "access_token": "access",
                    "account_id": "account-id",
                    "id_token": "not-a-jwt"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(credentials.account_display_name, None);
    }

    #[test]
    fn normalizes_codex_rate_limits() {
        let payload = json!({
            "account_id": "external-account",
            "email": "user@example.com",
            "plan_type": "prolite",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 1486,
                    "reset_at": 1781233774,
                    "used_percent": 23
                },
                "secondary_window": {
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 588286,
                    "reset_at": 1781820574,
                    "used_percent": 4
                }
            },
            "additional_rate_limits": [
                {
                    "limit_name": "GPT-5.3-Codex-Spark",
                    "metered_feature": "codex_bengalfox",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "limit_window_seconds": 18000,
                            "reset_after_seconds": 18000,
                            "reset_at": 1781250288,
                            "used_percent": 0
                        },
                        "secondary_window": {
                            "limit_window_seconds": 604800,
                            "reset_after_seconds": 398008,
                            "reset_at": 1781630296,
                            "used_percent": 0
                        }
                    }
                }
            ],
            "credits": {
                "balance": "0",
                "has_credits": false,
                "unlimited": false
            },
            "rate_limit_reset_credits": {
                "available_count": 1
            }
        });

        let snapshot = normalize_usage(&payload, Some("Codex"))
            .unwrap()
            .into_snapshot(AccountId::new("acct"));
        assert_eq!(snapshot.windows.len(), 5);

        let session = find_window(&snapshot.windows, "codex_session");
        assert_eq!(session.label, "Codex session");
        assert!(matches!(session.kind, UsageWindowKind::Session));
        assert_eq!(session.percent_used, Some(23.0));
        assert_eq!(session.percent_remaining, Some(77.0));
        assert_eq!(session.reset_at.unwrap().timestamp(), 1781233774);

        let weekly = find_window(&snapshot.windows, "codex_weekly");
        assert_eq!(weekly.label, "Codex weekly");
        assert!(matches!(weekly.kind, UsageWindowKind::Weekly));
        assert_eq!(weekly.percent_used, Some(4.0));
        assert_eq!(weekly.percent_remaining, Some(96.0));
        assert_eq!(weekly.reset_at.unwrap().timestamp(), 1781820574);

        let additional_session = find_window(&snapshot.windows, "codex_additional_0_session");
        assert_eq!(additional_session.label, "GPT-5.3-Codex-Spark session");
        assert_eq!(additional_session.percent_used, Some(0.0));

        let credits = find_window(&snapshot.windows, "codex_credits");
        assert_eq!(credits.label, "Codex credits");
        assert!(matches!(credits.kind, UsageWindowKind::Credits));
        assert_eq!(credits.remaining.as_ref().unwrap().value, 0.0);
        assert!(credits.limit.is_none());

        assert_eq!(snapshot.metadata["plan_type"], "prolite");
        assert_eq!(snapshot.metadata["email"], "user@example.com");
        assert_eq!(snapshot.metadata["credits_has_credits"], false);
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits_available_count"],
            1.0
        );
    }

    #[test]
    fn normalizes_app_server_rate_limits_with_reset_credit_expiry() {
        let payload = json!({
            "account_read": {
                "account": {
                    "type": "chatgpt",
                    "email": "user@example.com",
                    "planType": "prolite"
                },
                "requiresOpenaiAuth": true
            },
            "rate_limits_read": {
                "rateLimits": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": {
                        "usedPercent": 7,
                        "windowDurationMins": 300,
                        "resetsAt": 1783626874
                    },
                    "secondary": {
                        "usedPercent": 39,
                        "windowDurationMins": 10080,
                        "resetsAt": 1784040385
                    },
                    "credits": {
                        "hasCredits": false,
                        "unlimited": false,
                        "balance": "0"
                    },
                    "planType": "prolite",
                    "rateLimitReachedType": null
                },
                "rateLimitsByLimitId": {
                    "codex_bengalfox": {
                        "limitId": "codex_bengalfox",
                        "limitName": "GPT-5.3-Codex-Spark",
                        "primary": {
                            "usedPercent": 0,
                            "windowDurationMins": 300,
                            "resetsAt": 1783627252
                        },
                        "secondary": {
                            "usedPercent": 0,
                            "windowDurationMins": 10080,
                            "resetsAt": 1784214052
                        },
                        "credits": null,
                        "planType": "prolite",
                        "rateLimitReachedType": null
                    },
                    "codex": {
                        "limitId": "codex",
                        "limitName": null,
                        "primary": {
                            "usedPercent": 7,
                            "windowDurationMins": 300,
                            "resetsAt": 1783626874
                        },
                        "secondary": {
                            "usedPercent": 39,
                            "windowDurationMins": 10080,
                            "resetsAt": 1784040385
                        },
                        "credits": {
                            "hasCredits": false,
                            "unlimited": false,
                            "balance": "0"
                        },
                        "planType": "prolite",
                        "rateLimitReachedType": null
                    }
                },
                "rateLimitResetCredits": {
                    "availableCount": 4,
                    "credits": [
                        {
                            "id": "RateLimitResetCredit_old",
                            "resetType": "codexRateLimits",
                            "status": "available",
                            "grantedAt": 1781230493,
                            "expiresAt": 1783822493,
                            "title": "Full reset (Weekly + 5 hr)",
                            "description": "Thanks for using Codex!"
                        },
                        {
                            "id": "RateLimitResetCredit_new",
                            "resetType": "codexRateLimits",
                            "status": "available",
                            "grantedAt": 1781743124,
                            "expiresAt": 1784335124,
                            "title": "Full reset (Weekly + 5 hr)",
                            "description": "Thanks for using Codex!"
                        }
                    ]
                }
            }
        });

        let snapshot = normalize_app_server_usage(&payload, Some("Codex"))
            .unwrap()
            .into_snapshot(AccountId::new("acct"));
        assert_eq!(snapshot.windows.len(), 5);

        let session = find_window(&snapshot.windows, "codex_session");
        assert_eq!(session.percent_used, Some(7.0));
        assert_eq!(session.percent_remaining, Some(93.0));
        assert_eq!(session.reset_at.unwrap().timestamp(), 1783626874);

        let weekly = find_window(&snapshot.windows, "codex_weekly");
        assert_eq!(weekly.percent_used, Some(39.0));
        assert_eq!(weekly.reset_at.unwrap().timestamp(), 1784040385);

        let additional_session = find_window(&snapshot.windows, "codex_additional_0_session");
        assert_eq!(additional_session.label, "GPT-5.3-Codex-Spark session");

        let credits = find_window(&snapshot.windows, "codex_credits");
        assert_eq!(credits.remaining.as_ref().unwrap().value, 0.0);

        assert_eq!(
            snapshot.metadata["collection_mode"],
            "codex_app_server_rate_limits"
        );
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits_available_count"],
            4.0
        );
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits"]["next_expires_at"],
            1783822493.0
        );
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits"]["next_expires_at_iso"],
            "2026-07-12T02:14:53+00:00"
        );
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits"]["credits"][0]["expires_at"],
            1783822493.0
        );
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits"]["credits"][0]["expires_at_iso"],
            "2026-07-12T02:14:53+00:00"
        );
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits"]["credits"][0]["id"],
            "RateLimitResetCredit_old"
        );
        assert_eq!(snapshot.metadata["plan_type"], "prolite");
    }

    #[test]
    fn rejects_non_object_payloads() {
        let err = normalize_usage(&json!([1, 2, 3]), None).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::Parse);
    }

    #[test]
    fn reads_current_token_count_event_shape() {
        let event = json!({
            "timestamp": "2026-06-12T19:11:08.807Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 1000,
                        "cached_input_tokens": 100,
                        "output_tokens": 50
                    }
                }
            }
        });

        let info = codex_token_count_info(&event).expect("token_count info");
        let totals = codex_totals_from_value(&info["last_token_usage"]).expect("token totals");
        assert_eq!(totals.input, 1000);
        assert_eq!(totals.cached, 100);
        assert_eq!(totals.output, 50);
        assert_eq!(
            codex_event_timestamp(&event).unwrap().timestamp(),
            1_781_291_468
        );
    }

    #[test]
    fn reads_turn_context_model_shapes() {
        let current = json!({
            "type": "turn_context",
            "payload": { "model": "gpt-5.5" }
        });
        let nested = json!({
            "type": "event_msg",
            "payload": {
                "type": "turn_context",
                "payload": { "model": "gpt-5.4-mini" }
            }
        });

        assert_eq!(codex_turn_context_model(&current), Some("gpt-5.5"));
        assert_eq!(codex_turn_context_model(&nested), Some("gpt-5.4-mini"));
    }

    #[test]
    fn prices_codex_tokens_with_cache_and_model_normalization() {
        let cost = codex_cost_usd(
            "openai/gpt-5.5-2026-06-01",
            CodexTokenTotals {
                input: 1000,
                cached: 400,
                output: 100,
            },
        )
        .unwrap();

        assert_eq!(
            normalize_codex_model("openai/gpt-5.5-2026-06-01"),
            "gpt-5.5"
        );
        assert!((cost - 0.0062).abs() < f64::EPSILON);
    }

    #[test]
    fn token_total_does_not_double_count_cached_input() {
        let totals = CodexTokenTotals {
            input: 1_000,
            cached: 800,
            output: 100,
        };

        assert_eq!(totals.total(), 1_100);
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
