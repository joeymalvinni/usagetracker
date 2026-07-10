//! Codex collection orchestration and profile management.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};
use usage_core::ProviderId;

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{
        read_response_body, DailyUsageBucket, DiscoveredAccount, ProviderCollectionResult,
        ProviderCollector, ProviderError, ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT,
        HTTP_REQUEST_TIMEOUT,
    },
};

pub const PROVIDER_ID: &str = "codex";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_PERCENT: f64 = 100.0;
const COST_LOOKBACK_DAYS: u64 = 30;
const CODEX_APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);
const CODEX_ACCOUNT_USAGE_GRACE_TIMEOUT: Duration = Duration::from_secs(5);
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

        let body = read_response_body(response, "Codex usage response").await?;
        let payload: Value = serde_json::from_slice(&body).map_err(|err| {
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
        let local_codex_home = self.local_codex_home.clone();
        let profile_codex_home = profile.codex_home.clone();
        let profile_account_id = credentials.account_id.clone();
        let cost_started = Instant::now();
        debug!("codex local cost scan started");
        match tokio::task::spawn_blocking(move || {
            let local_account_id = codex_account_id_from_auth_file(&local_codex_home);
            let cost_roots = codex_session_roots(
                &profile_codex_home,
                &local_codex_home,
                local_account_id.as_deref(),
                &profile_account_id,
            );
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

mod app_server;
mod cost;
mod rate_limits;

use app_server::collect_usage_from_app_server;
use cost::{
    codex_account_id_from_auth_file, codex_session_roots, scan_codex_local_costs_cached,
    CodexCostCache, CodexUsageCostExt,
};
use rate_limits::{normalize_usage, number_from_json_value};

#[cfg(test)]
mod tests;
