use std::{path::PathBuf, time::Duration};

use chrono::{DateTime, TimeDelta, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use usage_core::{
    AccountId, ProviderId, UsageAmount, UsageSnapshot, UsageUnit, UsageWindow, UsageWindowKind,
};

use crate::providers::{
    DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
    ProviderErrorKind,
};

const CODEX_PROVIDER_ID: &str = "codex";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

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
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
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

        let auth: CodexAuth = serde_json::from_str(&contents).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Codex auth file is not valid JSON",
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

impl ProviderCollector for CodexCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(CODEX_PROVIDER_ID)
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
                external_account_id: credentials.account_id,
                display_name: Some("Codex".to_string()),
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
                .map_err(|_| {
                    ProviderError::new(ProviderErrorKind::Network, "Codex usage request failed")
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

            let payload: Value = response.json().await.map_err(|_| {
                ProviderError::new(ProviderErrorKind::Parse, "Codex usage JSON was invalid")
            })?;
            let snapshot = normalize_usage(
                &payload,
                AccountId::new(account.external_account_id.clone()),
                account.display_name.as_deref(),
            )?;

            Ok(ProviderCollectionResult {
                snapshot,
                collection_mode: "wham_usage_api".to_string(),
                raw_payload: self.capture_raw_payloads.then_some(payload),
                warnings: Vec::new(),
            })
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
    account_id: AccountId,
    display_name: Option<&str>,
) -> Result<UsageSnapshot, ProviderError> {
    let object = payload.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Codex usage response was not a JSON object",
        )
    })?;

    let mut windows = Vec::new();
    collect_codex_rate_limit_windows(&mut windows, "codex", "Codex", object.get("rate_limit"));
    collect_codex_rate_limit_windows(
        &mut windows,
        "codex_code_review",
        "Codex code review",
        object.get("code_review_rate_limit"),
    );
    collect_additional_rate_limit_windows(&mut windows, object.get("additional_rate_limits"));
    collect_codex_credits_window(&mut windows, object.get("credits"));

    let top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    Ok(UsageSnapshot {
        provider_id: ProviderId::new(CODEX_PROVIDER_ID),
        account_id,
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

fn collect_codex_rate_limit_windows(
    windows: &mut Vec<UsageWindow>,
    id_prefix: &str,
    label_prefix: &str,
    rate_limit: Option<&Value>,
) {
    let Some(rate_limit) = rate_limit.and_then(Value::as_object) else {
        return;
    };

    if let Some(window) = rate_limit_window(
        &format!("{id_prefix}_session"),
        &format!("{label_prefix} session"),
        UsageWindowKind::Session,
        rate_limit.get("primary_window"),
    ) {
        windows.push(window);
    }

    if let Some(window) = rate_limit_window(
        &format!("{id_prefix}_weekly"),
        &format!("{label_prefix} weekly"),
        UsageWindowKind::Weekly,
        rate_limit.get("secondary_window"),
    ) {
        windows.push(window);
    }
}

fn collect_additional_rate_limit_windows(windows: &mut Vec<UsageWindow>, value: Option<&Value>) {
    let Some(rate_limits) = value.and_then(Value::as_array) else {
        return;
    };

    for (index, rate_limit) in rate_limits.iter().enumerate() {
        let Some(rate_limit) = rate_limit.as_object() else {
            continue;
        };
        let label = rate_limit
            .get("limit_name")
            .and_then(Value::as_str)
            .or_else(|| rate_limit.get("metered_feature").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("Codex additional limit {}", index + 1));
        collect_codex_rate_limit_windows(
            windows,
            &format!("codex_additional_{index}"),
            &label,
            rate_limit.get("rate_limit"),
        );
    }
}

fn collect_codex_credits_window(windows: &mut Vec<UsageWindow>, credits: Option<&Value>) {
    let Some(credits) = credits.and_then(Value::as_object) else {
        return;
    };

    let balance = credits.get("balance").and_then(number_from_json_value);
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if balance.is_none() && !unlimited {
        return;
    }

    let remaining = if unlimited { None } else { balance };
    let mut metadata_label = "Codex credits".to_string();
    if unlimited {
        metadata_label.push_str(" (unlimited)");
    }

    windows.push(UsageWindow {
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
    });
}

fn rate_limit_window(
    window_id: &str,
    label: &str,
    kind: UsageWindowKind,
    value: Option<&Value>,
) -> Option<UsageWindow> {
    let object = value?.as_object()?;
    let percent_used = object
        .get("used_percent")
        .and_then(number_from_json_value)
        .map(|value| value.clamp(0.0, 100.0));
    let percent_remaining = percent_used.map(|value| 100.0 - value);
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
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
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

        let snapshot = normalize_usage(&payload, AccountId::new("acct"), Some("Codex")).unwrap();
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
        let err = normalize_usage(&json!([1, 2, 3]), AccountId::new("acct"), None).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::Parse);
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
