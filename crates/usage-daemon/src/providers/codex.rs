use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{
    DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
    ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
};

pub const PROVIDER_ID: &str = "codex";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_PERCENT: f64 = 100.0;

#[derive(Clone)]
pub struct CodexCollector {
    auth_path: PathBuf,
    client: reqwest::Client,
    capture_raw_payloads: bool,
}

impl CodexCollector {
    pub fn new(capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Codex auth"))?;
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent("codex-cli")
            .build()?;
        Ok(Self {
            auth_path: home.join(".codex/auth.json"),
            client,
            capture_raw_payloads,
        })
    }

    async fn load_credentials(&self) -> Result<CodexCredentials, ProviderError> {
        let contents = tokio::fs::read_to_string(&self.auth_path)
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsMissing,
                        "~/.codex/auth.json is missing",
                    )
                } else {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        "failed to read Codex auth file",
                    )
                }
            })?;

        let auth: CodexAuth = serde_json::from_str(&contents).map_err(|err| {
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

        Ok(CodexCredentials {
            access_token: tokens.access_token,
            account_id: tokens.account_id,
        })
    }
}

#[async_trait]
impl ProviderCollector for CodexCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        let credentials = self.load_credentials().await?;
        Ok(vec![DiscoveredAccount {
            external_account_id: credentials.account_id,
            display_name: Some("Codex".to_string()),
        }])
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let credentials = self.load_credentials().await?;
        if credentials.account_id != account.external_account_id {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Codex account changed since discovery",
            ));
        }

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
        let usage = normalize_usage(&payload, account.display_name.as_deref())?;

        Ok(ProviderCollectionResult {
            usage,
            collection_mode: "wham_usage_api".to_string(),
            raw_payload: self.capture_raw_payloads.then_some(payload),
            warnings: Vec::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: String,
    account_id: String,
}

#[derive(Debug)]
struct CodexCredentials {
    access_token: String,
    account_id: String,
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
        assert_eq!(snapshot.metadata["credits_has_credits"], false);
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits_available_count"],
            1.0
        );
    }

    #[test]
    fn rejects_non_object_payloads() {
        let err = normalize_usage(&json!([1, 2, 3]), None).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::Parse);
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
