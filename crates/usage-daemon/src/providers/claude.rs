use std::{path::PathBuf, time::Duration};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use keyring::{Entry, Error as KeyringError};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use usage_core::{
    AccountId, ProviderId, UsageAmount, UsageSnapshot, UsageUnit, UsageWindow, UsageWindowKind,
};

use crate::providers::{
    DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
    ProviderErrorKind,
};

const CLAUDE_PROVIDER_ID: &str = "claude";
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const CLAUDE_CREDENTIALS_FILE: &str = ".claude/.credentials.json";
const CLAUDE_COLLECTION_MODE: &str = "oauth_usage_api";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
const TOKEN_REFRESH_SKEW_MS: i64 = 60_000;

#[derive(Clone)]
pub struct ClaudeCollector {
    keychain_account: String,
    credentials_file_path: PathBuf,
    client: reqwest::Client,
    capture_raw_payloads: bool,
}

impl ClaudeCollector {
    pub fn new(capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Claude data"))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent("usage-daemon")
            .build()?;
        Ok(Self {
            keychain_account: std::env::var("USER").unwrap_or_else(|_| "default".to_string()),
            credentials_file_path: home.join(CLAUDE_CREDENTIALS_FILE),
            client,
            capture_raw_payloads,
        })
    }

    async fn load_credentials(&self) -> Result<ClaudeCredentials, ProviderError> {
        let keychain_account = self.keychain_account.clone();
        let credentials_file_path = self.credentials_file_path.clone();
        tokio::task::spawn_blocking(move || {
            load_credentials_from_keychain_or_file(&keychain_account, credentials_file_path)
        })
        .await
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Claude credential load task failed",
            )
        })?
    }

    async fn load_with_auto_refresh(&self) -> Result<ClaudeCredentials, ProviderError> {
        let credentials = self.load_credentials().await?;
        if credentials.is_expired() {
            self.refresh_credentials(credentials).await
        } else {
            Ok(credentials)
        }
    }

    async fn refresh_credentials(
        &self,
        credentials: ClaudeCredentials,
    ) -> Result<ClaudeCredentials, ProviderError> {
        if credentials.refresh_token.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Claude OAuth credentials are missing a refresh token",
            ));
        }

        let response = self
            .client
            .post(CLAUDE_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", credentials.refresh_token.as_str()),
                ("client_id", CLAUDE_OAUTH_CLIENT_ID),
            ])
            .send()
            .await
            .map_err(|_| {
                ProviderError::new(ProviderErrorKind::Network, "Claude token refresh failed")
            })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "Claude token refresh is rate limited",
            ));
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                "Claude refresh token was rejected",
            ));
        }
        if !status.is_success() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("Claude token refresh returned HTTP {}", status.as_u16()),
            ));
        }

        let refresh: TokenRefreshResponse = response.json().await.map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Claude token refresh JSON was invalid",
            )
        })?;

        if refresh.access_token.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                "Claude token refresh response did not include an access token",
            ));
        }

        let refreshed = credentials.with_refreshed_tokens(refresh)?;
        self.save_credentials(refreshed.clone()).await?;
        Ok(refreshed)
    }

    async fn save_credentials(&self, credentials: ClaudeCredentials) -> Result<(), ProviderError> {
        match credentials.source.clone() {
            CredentialSource::Keychain => {
                let keychain_account = self.keychain_account.clone();
                let contents = credentials.raw.to_string();
                tokio::task::spawn_blocking(move || {
                    save_keychain_credentials(&keychain_account, &contents)
                })
                .await
                .map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        "Claude credential save task failed",
                    )
                })?
            }
            CredentialSource::File(path) => {
                let contents = serde_json::to_vec_pretty(&credentials.raw).map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        "failed to serialize refreshed Claude credentials",
                    )
                })?;
                tokio::fs::write(path, contents).await.map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        "failed to save refreshed Claude credentials file",
                    )
                })
            }
        }
    }

    async fn fetch_usage(&self, credentials: &ClaudeCredentials) -> Result<Value, ProviderError> {
        let response = self
            .client
            .get(CLAUDE_USAGE_URL)
            .bearer_auth(&credentials.access_token)
            .header("anthropic-beta", CLAUDE_OAUTH_BETA)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| {
                ProviderError::new(ProviderErrorKind::Network, "Claude usage request failed")
            })?;

        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                "Claude OAuth credentials were rejected",
            ));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "Claude usage endpoint is rate limited",
            ));
        }
        if !status.is_success() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("Claude usage endpoint returned HTTP {}", status.as_u16()),
            ));
        }

        response.json().await.map_err(|_| {
            ProviderError::new(ProviderErrorKind::Parse, "Claude usage JSON was invalid")
        })
    }
}

impl ProviderCollector for ClaudeCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(CLAUDE_PROVIDER_ID)
    }

    fn discover_accounts<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<DiscoveredAccount>, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let credentials = self.load_credentials().await?;
            Ok(vec![DiscoveredAccount {
                external_account_id: credentials.account_id(),
                display_name: Some(credentials.display_name()),
            }])
        })
    }

    fn collect_usage<'a>(
        &'a self,
        account: &'a DiscoveredAccount,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ProviderCollectionResult, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut credentials = self.load_with_auto_refresh().await?;
            if credentials.account_id() != account.external_account_id {
                return Err(ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "Claude account changed since discovery",
                ));
            }

            let payload = match self.fetch_usage(&credentials).await {
                Err(err) if err.kind() == ProviderErrorKind::Unauthorized => {
                    credentials = self.refresh_credentials(credentials).await?;
                    self.fetch_usage(&credentials).await?
                }
                result => result?,
            };
            let snapshot = normalize_usage(
                &payload,
                &credentials,
                AccountId::new(account.external_account_id.clone()),
            )?;

            Ok(ProviderCollectionResult {
                snapshot,
                collection_mode: CLAUDE_COLLECTION_MODE.to_string(),
                raw_payload: self.capture_raw_payloads.then_some(payload),
                warnings: Vec::new(),
            })
        })
    }
}

fn load_credentials_from_keychain_or_file(
    keychain_account: &str,
    credentials_file_path: PathBuf,
) -> Result<ClaudeCredentials, ProviderError> {
    match load_keychain_credentials(keychain_account) {
        Ok(credentials) => Ok(credentials),
        Err(err) if err.kind() == ProviderErrorKind::CredentialsMissing => {
            load_file_credentials(&credentials_file_path, keychain_account)
        }
        Err(err) => Err(err),
    }
}

fn load_keychain_credentials(keychain_account: &str) -> Result<ClaudeCredentials, ProviderError> {
    let entry = Entry::new(CLAUDE_KEYCHAIN_SERVICE, keychain_account).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "failed to create Claude Keychain entry",
        )
    })?;

    let password = entry.get_password().map_err(|err| match err {
        KeyringError::NoEntry => ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            "Claude Code credentials are missing from macOS Keychain",
        ),
        _ => ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "failed to read Claude Code credentials from macOS Keychain",
        ),
    })?;

    parse_credentials(&password, keychain_account, CredentialSource::Keychain)
}

fn save_keychain_credentials(keychain_account: &str, contents: &str) -> Result<(), ProviderError> {
    let entry = Entry::new(CLAUDE_KEYCHAIN_SERVICE, keychain_account).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "failed to create Claude Keychain entry",
        )
    })?;
    entry.set_password(contents).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "failed to save Claude Code credentials to macOS Keychain",
        )
    })
}

fn load_file_credentials(
    path: &PathBuf,
    keychain_account: &str,
) -> Result<ClaudeCredentials, ProviderError> {
    let contents = std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                "Claude credentials are missing from Keychain and ~/.claude/.credentials.json",
            )
        } else {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "failed to read Claude credentials file",
            )
        }
    })?;
    parse_credentials(
        &contents,
        keychain_account,
        CredentialSource::File(path.clone()),
    )
}

fn parse_credentials(
    contents: &str,
    keychain_account: impl Into<String>,
    source: CredentialSource,
) -> Result<ClaudeCredentials, ProviderError> {
    let raw: Value = serde_json::from_str(contents).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude OAuth credentials are not valid JSON",
        )
    })?;
    let auth: ClaudeKeychainAuth = serde_json::from_value(raw.clone()).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude OAuth credentials have an invalid shape",
        )
    })?;
    let oauth = auth.claude_ai_oauth.ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude OAuth credentials are missing OAuth data",
        )
    })?;

    if oauth.access_token.trim().is_empty() || oauth.refresh_token.trim().is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude OAuth credentials are missing token fields",
        ));
    }

    Ok(ClaudeCredentials {
        keychain_account: keychain_account.into(),
        source,
        access_token: oauth.access_token,
        refresh_token: oauth.refresh_token,
        subscription_type: oauth.subscription_type,
        rate_limit_tier: oauth.rate_limit_tier,
        expires_at_ms: oauth.expires_at,
        scopes: oauth.scopes,
        raw,
    })
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Keychain,
    File(PathBuf),
}

#[derive(Debug, Deserialize)]
struct ClaudeKeychainAuth {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeAiOauth>,
}

#[derive(Debug, Deserialize)]
struct ClaudeAiOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenRefreshResponse {
    #[serde(rename = "access_token")]
    access_token: String,
    #[serde(rename = "refresh_token")]
    refresh_token: Option<String>,
    #[serde(rename = "expires_in")]
    expires_in: i64,
    #[serde(rename = "token_type")]
    token_type: Option<String>,
}

#[derive(Clone, Debug)]
struct ClaudeCredentials {
    keychain_account: String,
    source: CredentialSource,
    access_token: String,
    refresh_token: String,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
    expires_at_ms: Option<i64>,
    scopes: Vec<String>,
    raw: Value,
}

impl ClaudeCredentials {
    fn account_id(&self) -> String {
        self.keychain_account.clone()
    }

    fn display_name(&self) -> String {
        match self.subscription_type.as_deref() {
            Some(subscription_type) if !subscription_type.trim().is_empty() => {
                format!("Claude {subscription_type}")
            }
            _ => "Claude".to_string(),
        }
    }

    fn is_expired(&self) -> bool {
        self.expires_at_ms.is_some_and(|expires_at| {
            expires_at <= Utc::now().timestamp_millis() + TOKEN_REFRESH_SKEW_MS
        })
    }

    fn with_refreshed_tokens(
        mut self,
        refresh: TokenRefreshResponse,
    ) -> Result<Self, ProviderError> {
        let refresh_token = refresh
            .refresh_token
            .unwrap_or_else(|| self.refresh_token.clone());
        let expires_at_ms = Utc::now().timestamp_millis() + refresh.expires_in.saturating_mul(1000);

        update_oauth_field(
            &mut self.raw,
            "accessToken",
            Value::String(refresh.access_token.clone()),
        )?;
        update_oauth_field(
            &mut self.raw,
            "refreshToken",
            Value::String(refresh_token.clone()),
        )?;
        update_oauth_field(&mut self.raw, "expiresAt", json!(expires_at_ms))?;
        if let Some(token_type) = refresh.token_type {
            update_oauth_field(&mut self.raw, "tokenType", Value::String(token_type))?;
        }

        self.access_token = refresh.access_token;
        self.refresh_token = refresh_token;
        self.expires_at_ms = Some(expires_at_ms);
        Ok(self)
    }
}

fn update_oauth_field(raw: &mut Value, field: &str, value: Value) -> Result<(), ProviderError> {
    let Some(oauth) = raw.get_mut("claudeAiOauth").and_then(Value::as_object_mut) else {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude OAuth credentials are missing OAuth data",
        ));
    };
    oauth.insert(field.to_string(), value);
    Ok(())
}

fn normalize_usage(
    payload: &Value,
    credentials: &ClaudeCredentials,
    account_id: AccountId,
) -> Result<UsageSnapshot, ProviderError> {
    let object = payload.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Claude usage response was not a JSON object",
        )
    })?;

    let mut windows = Vec::new();
    collect_utilization_windows(&mut windows, payload, &[]);
    collect_extra_usage_window(&mut windows, object.get("extra_usage"));

    if windows.is_empty() {
        let top_level_keys = object.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Claude usage response did not contain usage windows; top-level keys: {top_level_keys}"),
        ));
    }

    let top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    Ok(UsageSnapshot {
        provider_id: ProviderId::new(CLAUDE_PROVIDER_ID),
        account_id,
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "collection_mode": CLAUDE_COLLECTION_MODE,
            "keychain_service": CLAUDE_KEYCHAIN_SERVICE,
            "keychain_account": credentials.keychain_account,
            "subscription_type": credentials.subscription_type,
            "rate_limit_tier": credentials.rate_limit_tier,
            "token_expires_at_ms": credentials.expires_at_ms,
            "scopes": credentials.scopes,
            "extra_usage_enabled": object
                .get("extra_usage")
                .and_then(|value| value.get("is_enabled").or_else(|| value.get("enabled")))
                .and_then(Value::as_bool),
            "top_level_keys": top_level_keys,
        }),
    })
}

fn collect_utilization_windows(windows: &mut Vec<UsageWindow>, value: &Value, path: &[String]) {
    match value {
        Value::Number(_) | Value::String(_)
            if path
                .iter()
                .any(|key| key.to_ascii_lowercase().contains("utilization")) =>
        {
            if let Some(percent_used) = number_from_json_value(value) {
                windows.push(percent_window(path, None, percent_used, None));
            }
        }
        Value::Number(_) | Value::String(_) => {}
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let mut child_path = path.to_vec();
                child_path.push(index.to_string());
                collect_utilization_windows(windows, value, &child_path);
            }
        }
        Value::Object(object) => {
            if let Some(percent_used) = utilization_percent(object) {
                let reset_at = reset_at_from_object(object);
                let label = object_label(object).or_else(|| path.last().cloned());
                windows.push(percent_window(path, label, percent_used, reset_at));
                return;
            }

            for (key, value) in object {
                if key.to_ascii_lowercase().contains("utilization") {
                    if let Some(percent_used) = number_from_json_value(value) {
                        windows.push(percent_window(
                            &[path, std::slice::from_ref(key)].concat(),
                            None,
                            percent_used,
                            reset_at_from_object(object),
                        ));
                        continue;
                    }
                }
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                collect_utilization_windows(windows, value, &child_path);
            }
        }
        _ => {}
    }
}

fn percent_window(
    path: &[String],
    label: Option<String>,
    percent_used: f64,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let percent_used = percent_used.clamp(0.0, 100.0);
    let percent_remaining = 100.0 - percent_used;
    let window_name = path.last().map_or("claude", String::as_str);

    UsageWindow {
        window_id: format!("claude_usage_{}", stable_window_fragment(&path.join("_"))),
        label: label.unwrap_or_else(|| humanize_window_label(window_name)),
        kind: usage_kind_from_name(window_name),
        used: Some(UsageAmount {
            value: percent_used,
            unit: UsageUnit::Percent,
        }),
        limit: Some(UsageAmount {
            value: 100.0,
            unit: UsageUnit::Percent,
        }),
        remaining: Some(UsageAmount {
            value: percent_remaining,
            unit: UsageUnit::Percent,
        }),
        percent_used: Some(percent_used),
        percent_remaining: Some(percent_remaining),
        reset_at,
    }
}

fn collect_extra_usage_window(windows: &mut Vec<UsageWindow>, value: Option<&Value>) {
    let Some(object) = value.and_then(Value::as_object) else {
        return;
    };

    let used = first_number(
        object,
        &[
            "current_usage",
            "currentUsage",
            "used",
            "usage",
            "spent",
            "spent_usd",
        ],
    );
    let limit = first_number(
        object,
        &[
            "monthly_limit",
            "monthlyLimit",
            "limit",
            "spend_limit",
            "spendLimit",
        ],
    );

    if used.is_none() && limit.is_none() {
        return;
    }

    let remaining = used.zip(limit).map(|(used, limit)| (limit - used).max(0.0));
    let percent_used = used
        .zip(limit)
        .filter(|(_, limit)| *limit > 0.0)
        .map(|(used, limit)| (used / limit * 100.0).clamp(0.0, 100.0));

    windows.push(UsageWindow {
        window_id: "claude_extra_usage".to_string(),
        label: "Claude extra usage".to_string(),
        kind: UsageWindowKind::Credits,
        used: used.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        limit: limit.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        remaining: remaining.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        percent_used,
        percent_remaining: percent_used.map(|value| 100.0 - value),
        reset_at: reset_at_from_object(object),
    });
}

fn utilization_percent(object: &Map<String, Value>) -> Option<f64> {
    first_number(
        object,
        &[
            "utilization",
            "used_percent",
            "usedPercent",
            "percent_used",
            "percentUsed",
        ],
    )
}

fn reset_at_from_object(object: &Map<String, Value>) -> Option<DateTime<Utc>> {
    [
        "resets_at",
        "resetsAt",
        "reset_at",
        "resetAt",
        "reset_date",
        "resetDate",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(date_time_from_json_value))
}

fn object_label(object: &Map<String, Value>) -> Option<String> {
    [
        "label",
        "name",
        "title",
        "rate_limit_type",
        "rateLimitType",
        "claim",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(Value::as_str))
    .map(humanize_window_label)
}

fn first_number(object: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(number_from_json_value))
}

fn number_from_json_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn date_time_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(value) => DateTime::parse_from_rfc3339(value)
            .map(|value| value.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|value| value.and_utc())
            }),
        Value::Number(_) => {
            let timestamp = number_from_json_value(value)?;
            if timestamp > 10_000_000_000.0 {
                Utc.timestamp_millis_opt(timestamp.round() as i64).single()
            } else {
                Utc.timestamp_opt(timestamp.round() as i64, 0).single()
            }
        }
        _ => None,
    }
}

fn usage_kind_from_name(name: &str) -> UsageWindowKind {
    let name = name.to_ascii_lowercase();
    if name.contains("session") || name.contains("hour") {
        UsageWindowKind::Session
    } else if name.contains("daily") || name.contains("day") {
        UsageWindowKind::Daily
    } else if name.contains("weekly") || name.contains("week") {
        UsageWindowKind::Weekly
    } else if name.contains("monthly") || name.contains("month") {
        UsageWindowKind::Monthly
    } else {
        UsageWindowKind::Other(name)
    }
}

fn humanize_window_label(value: impl AsRef<str>) -> String {
    let value = value.as_ref().replace(['_', '-'], " ");
    let value = value.trim();
    if value.to_ascii_lowercase().starts_with("claude") {
        value.to_string()
    } else {
        format!("Claude {value}")
    }
}

fn stable_window_fragment(value: &str) -> String {
    value
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keychain_oauth_credentials() {
        let credentials = parse_credentials(
            r#"{
                "claudeAiOauth": {
                    "accessToken": "access",
                    "refreshToken": "refresh",
                    "expiresAt": 1780000000000,
                    "scopes": ["user:inference"],
                    "subscriptionType": "max",
                    "rateLimitTier": "standard"
                }
            }"#,
            "joey",
            CredentialSource::Keychain,
        )
        .unwrap();

        assert_eq!(credentials.account_id(), "joey");
        assert_eq!(credentials.display_name(), "Claude max");
        assert_eq!(credentials.access_token, "access");
        assert_eq!(credentials.refresh_token, "refresh");
        assert_eq!(credentials.subscription_type.as_deref(), Some("max"));
        assert_eq!(credentials.rate_limit_tier.as_deref(), Some("standard"));
        assert_eq!(credentials.expires_at_ms, Some(1780000000000_i64));
        assert_eq!(credentials.scopes, vec!["user:inference"]);
    }

    #[test]
    fn rejects_keychain_credentials_without_tokens() {
        let err = parse_credentials(
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"refresh","scopes":[]}}"#,
            "joey",
            CredentialSource::Keychain,
        )
        .unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::CredentialsInvalid);
    }

    #[test]
    fn updates_refreshed_tokens_in_raw_credentials() {
        let credentials = parse_credentials(
            r#"{
                "claudeAiOauth": {
                    "accessToken": "old-access",
                    "refreshToken": "old-refresh",
                    "expiresAt": 1000,
                    "scopes": [],
                    "subscriptionType": "team"
                }
            }"#,
            "joey",
            CredentialSource::Keychain,
        )
        .unwrap();

        let refreshed = credentials
            .with_refreshed_tokens(TokenRefreshResponse {
                access_token: "new-access".to_string(),
                refresh_token: Some("new-refresh".to_string()),
                expires_in: 3600,
                token_type: Some("bearer".to_string()),
            })
            .unwrap();

        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "new-refresh");
        assert_eq!(refreshed.raw["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(
            refreshed.raw["claudeAiOauth"]["refreshToken"],
            "new-refresh"
        );
        assert_eq!(refreshed.raw["claudeAiOauth"]["tokenType"], "bearer");
        assert!(refreshed.expires_at_ms.unwrap() > Utc::now().timestamp_millis());
    }

    #[test]
    fn normalizes_oauth_usage_utilization() {
        let credentials = test_credentials();
        let payload = json!({
            "utilization": {
                "five_hour": {
                    "utilization": 42.5,
                    "resets_at": "2026-06-12T08:00:00Z",
                    "rate_limit_type": "five hour"
                },
                "weekly": {
                    "usedPercent": 5,
                    "resetDate": "2026-06-18T22:09:34Z"
                }
            },
            "extra_usage": {
                "is_enabled": true,
                "current_usage": 12.5,
                "monthly_limit": 100.0
            }
        });

        let snapshot = normalize_usage(&payload, &credentials, AccountId::new("joey")).unwrap();
        assert_eq!(snapshot.provider_id, ProviderId::new("claude"));
        assert_eq!(snapshot.windows.len(), 3);

        let five_hour = find_window(&snapshot.windows, "claude_usage_utilization_five_hour");
        assert!(matches!(five_hour.kind, UsageWindowKind::Session));
        assert_eq!(five_hour.label, "Claude five hour");
        assert_eq!(five_hour.used.as_ref().unwrap().value, 42.5);
        assert!(matches!(
            five_hour.used.as_ref().unwrap().unit,
            UsageUnit::Percent
        ));
        assert_eq!(five_hour.limit.as_ref().unwrap().value, 100.0);
        assert_eq!(five_hour.remaining.as_ref().unwrap().value, 57.5);
        assert_eq!(five_hour.percent_used, Some(42.5));
        assert_eq!(five_hour.percent_remaining, Some(57.5));
        assert_eq!(five_hour.reset_at.unwrap().timestamp(), 1781251200);

        let weekly = find_window(&snapshot.windows, "claude_usage_utilization_weekly");
        assert!(matches!(weekly.kind, UsageWindowKind::Weekly));
        assert_eq!(weekly.percent_used, Some(5.0));
        assert_eq!(weekly.percent_remaining, Some(95.0));

        let extra = find_window(&snapshot.windows, "claude_extra_usage");
        assert!(matches!(extra.kind, UsageWindowKind::Credits));
        assert_eq!(extra.used.as_ref().unwrap().value, 12.5);
        assert_eq!(extra.limit.as_ref().unwrap().value, 100.0);
        assert_eq!(extra.remaining.as_ref().unwrap().value, 87.5);
        assert_eq!(extra.percent_used, Some(12.5));

        assert_eq!(snapshot.metadata["collection_mode"], CLAUDE_COLLECTION_MODE);
        assert_eq!(snapshot.metadata["subscription_type"], "team");
        assert_eq!(snapshot.metadata["extra_usage_enabled"], true);
    }

    #[test]
    fn normalizes_numeric_utilization_values() {
        let snapshot = normalize_usage(
            &json!({
                "utilization": {
                    "daily": 9.25
                }
            }),
            &test_credentials(),
            AccountId::new("joey"),
        )
        .unwrap();

        let daily = find_window(&snapshot.windows, "claude_usage_utilization_daily");
        assert!(matches!(daily.kind, UsageWindowKind::Daily));
        assert_eq!(daily.percent_used, Some(9.25));
        assert_eq!(daily.remaining.as_ref().unwrap().value, 90.75);
    }

    #[test]
    fn rejects_usage_without_windows() {
        let err =
            normalize_usage(&json!({}), &test_credentials(), AccountId::new("joey")).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::Parse);
    }

    fn test_credentials() -> ClaudeCredentials {
        ClaudeCredentials {
            keychain_account: "joey".to_string(),
            source: CredentialSource::Keychain,
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            subscription_type: Some("team".to_string()),
            rate_limit_tier: Some("default".to_string()),
            expires_at_ms: Some(1780000000000_i64),
            scopes: vec!["user:inference".to_string()],
            raw: json!({
                "claudeAiOauth": {
                    "accessToken": "access",
                    "refreshToken": "refresh",
                    "expiresAt": 1780000000000_i64,
                    "scopes": ["user:inference"],
                    "subscriptionType": "team",
                    "rateLimitTier": "default"
                }
            }),
        }
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
