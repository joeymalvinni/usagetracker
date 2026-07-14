use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::providers::{
    read_response_body, retry_after_deadline, ProviderError, ProviderErrorKind,
};

use super::credentials::{save_credentials, ClaudeCredentials, TokenRefreshResponse};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Clone)]
pub(super) struct ClaudeApiClient {
    client: reqwest::Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClaudeAccountIdentity {
    pub account_id: String,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeProfileResponse {
    account: ClaudeProfileAccount,
}

#[derive(Debug, Deserialize)]
struct ClaudeProfileAccount {
    uuid: String,
    #[serde(default, alias = "email_address")]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CachedClaudeProfile {
    #[serde(rename = "oauthAccount")]
    oauth_account: CachedClaudeAccount,
}

#[derive(Debug, Deserialize)]
struct CachedClaudeAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: String,
    #[serde(rename = "emailAddress", default)]
    email_address: Option<String>,
}

impl ClaudeApiClient {
    pub(super) fn new(connect_timeout: Duration, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(timeout)
            .user_agent("usage-daemon")
            .build()?;
        Ok(Self { client })
    }

    pub(super) async fn refresh_credentials(
        &self,
        credentials: ClaudeCredentials,
    ) -> Result<ClaudeCredentials, ProviderError> {
        if credentials.refresh_token.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Claude OAuth credentials are missing a refresh token",
            ));
        }

        let started = Instant::now();
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
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("Claude token refresh failed: {err}"),
                )
            })?;

        let status = response.status();
        let retry_at = retry_after_deadline(response.headers());
        let body = read_response_body(response, "Claude token refresh response").await?;
        let response_error_code = response_error_code(&body);
        debug!(
            provider_id = super::PROVIDER_ID,
            endpoint = "oauth_token_refresh",
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            response_bytes = body.len(),
            response_error_code = response_error_code.as_deref().unwrap_or("none"),
            "Claude OAuth response received"
        );
        if let Err(err) = map_refresh_error(status, response_error_code.as_deref(), retry_at) {
            warn!(
                provider_id = super::PROVIDER_ID,
                endpoint = "oauth_token_refresh",
                status = status.as_u16(),
                elapsed_ms = started.elapsed().as_millis(),
                response_bytes = body.len(),
                response_error_code = response_error_code.as_deref().unwrap_or("unknown"),
                error_code = err.kind().as_str(),
                error = %err,
                "Claude OAuth token refresh failed"
            );
            return Err(err);
        }
        let refresh: TokenRefreshResponse = serde_json::from_slice(&body).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Claude token refresh JSON was invalid: {err}"),
            )
        })?;

        if refresh.access_token.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                "Claude token refresh response did not include an access token",
            ));
        }

        let expected_contents = credentials.source_contents().to_string();
        let refreshed = credentials.with_refreshed_tokens(refresh)?;
        save_credentials(refreshed, expected_contents).await
    }

    pub(super) async fn fetch_usage(
        &self,
        credentials: &ClaudeCredentials,
    ) -> Result<Value, ProviderError> {
        let started = Instant::now();
        let response = self
            .client
            .get(CLAUDE_USAGE_URL)
            .bearer_auth(&credentials.access_token)
            .header("anthropic-beta", CLAUDE_OAUTH_BETA)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("Claude usage request failed: {err}"),
                )
            })?;

        let status = response.status();
        let retry_at = retry_after_deadline(response.headers());
        let body = read_response_body(response, "Claude usage response").await?;
        let response_error_code = response_error_code(&body);
        debug!(
            provider_id = super::PROVIDER_ID,
            endpoint = "oauth_usage",
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            response_bytes = body.len(),
            response_error_code = response_error_code.as_deref().unwrap_or("none"),
            "Claude OAuth response received"
        );
        if let Err(err) = map_usage_error(status, retry_at) {
            warn!(
                provider_id = super::PROVIDER_ID,
                endpoint = "oauth_usage",
                status = status.as_u16(),
                elapsed_ms = started.elapsed().as_millis(),
                response_bytes = body.len(),
                response_error_code = response_error_code.as_deref().unwrap_or("unknown"),
                error_code = err.kind().as_str(),
                error = %err,
                "Claude OAuth usage request failed"
            );
            return Err(err);
        }
        serde_json::from_slice(&body).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Claude usage JSON was invalid: {err}"),
            )
        })
    }

    pub(super) async fn fetch_profile(
        &self,
        credentials: &ClaudeCredentials,
    ) -> Result<ClaudeAccountIdentity, ProviderError> {
        let response = self
            .client
            .get(CLAUDE_PROFILE_URL)
            .bearer_auth(&credentials.access_token)
            .header("anthropic-beta", CLAUDE_OAUTH_BETA)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("Claude profile request failed: {err}"),
                )
            })?;

        let retry_at = retry_after_deadline(response.headers());
        map_profile_error(response.status(), retry_at)?;
        let body = read_response_body(response, "Claude profile response").await?;
        parse_profile_identity(&body)
    }
}

pub(super) fn parse_profile_identity(body: &[u8]) -> Result<ClaudeAccountIdentity, ProviderError> {
    let profile: ClaudeProfileResponse = serde_json::from_slice(body).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Claude profile JSON was invalid: {err}"),
        )
    })?;
    normalized_identity(
        profile.account.uuid,
        profile.account.email,
        "Claude profile",
    )
}

pub(super) fn parse_cached_profile_identity(
    body: &[u8],
) -> Result<ClaudeAccountIdentity, ProviderError> {
    let profile: CachedClaudeProfile = serde_json::from_slice(body).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("cached Claude profile JSON was invalid: {err}"),
        )
    })?;
    normalized_identity(
        profile.oauth_account.account_uuid,
        profile.oauth_account.email_address,
        "cached Claude profile",
    )
}

fn normalized_identity(
    account_id: String,
    email: Option<String>,
    source: &str,
) -> Result<ClaudeAccountIdentity, ProviderError> {
    let account_id = account_id.trim();
    let account_id = uuid::Uuid::parse_str(account_id)
        .map(|account_id| account_id.hyphenated().to_string())
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("{source} did not include a valid account UUID"),
            )
        })?;
    let email = email
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Ok(ClaudeAccountIdentity { account_id, email })
}

fn map_refresh_error(
    status: StatusCode,
    response_error_code: Option<&str>,
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), ProviderError> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProviderError::new(
            ProviderErrorKind::RateLimited,
            "Claude token refresh is rate limited",
        )
        .with_retry_at(retry_at));
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Claude refresh token was rejected",
        ));
    }
    if status == StatusCode::BAD_REQUEST && response_error_code == Some("invalid_grant") {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Claude refresh token was rejected (invalid_grant)",
        ));
    }
    if !status.is_success() {
        return Err(ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Claude token refresh returned HTTP {}", status.as_u16()),
        ));
    }
    Ok(())
}

fn response_error_code(body: &[u8]) -> Option<String> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    let code = [
        payload.pointer("/error/type"),
        payload.pointer("/error/code"),
        payload.get("error"),
        payload.get("error_code"),
        payload.get("type"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find_map(safe_error_code);
    code
}

fn safe_error_code(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '_' | '-' | '.')))
    .then(|| value.to_string())
}

fn map_usage_error(
    status: StatusCode,
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), ProviderError> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Claude OAuth credentials were rejected",
        )
        .with_retry_at(retry_at));
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
    Ok(())
}

fn map_profile_error(
    status: StatusCode,
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), ProviderError> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Claude OAuth profile credentials were rejected",
        )
        .with_retry_at(retry_at));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProviderError::new(
            ProviderErrorKind::RateLimited,
            "Claude profile endpoint is rate limited",
        ));
    }
    if !status.is_success() {
        return Err(ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Claude profile endpoint returned HTTP {}", status.as_u16()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oauth_profile_account_identity() {
        let identity = parse_profile_identity(
            br#"{
                "account": {
                    "uuid": " 986efbc1-2be6-407a-9bcc-2e429b8e358d ",
                    "email": " person@example.com "
                },
                "organization": {"uuid": "organization-id"}
            }"#,
        )
        .unwrap();

        assert_eq!(
            identity,
            ClaudeAccountIdentity {
                account_id: "986efbc1-2be6-407a-9bcc-2e429b8e358d".to_string(),
                email: Some("person@example.com".to_string()),
            }
        );
    }

    #[test]
    fn extracts_only_safe_oauth_error_codes() {
        assert_eq!(
            response_error_code(br#"{"error":"invalid_grant","error_description":"expired"}"#)
                .as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            response_error_code(
                br#"{"error":{"type":"authentication_error","message":"secret details"}}"#
            )
            .as_deref(),
            Some("authentication_error")
        );
        assert_eq!(
            response_error_code(br#"{"error":"unsafe code with spaces"}"#),
            None
        );
    }

    #[test]
    fn rejects_oauth_profile_without_account_uuid() {
        let error = parse_profile_identity(br#"{"account":{"uuid":"   "}}"#).unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::Parse);
        assert!(error.short_message().contains("account UUID"));
    }

    #[test]
    fn parses_cached_profile_account_identity() {
        let identity = parse_cached_profile_identity(
            br#"{
                "oauthAccount": {
                    "accountUuid": "986EFBC1-2BE6-407A-9BCC-2E429B8E358D",
                    "emailAddress": "person@example.com"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(identity.account_id, "986efbc1-2be6-407a-9bcc-2e429b8e358d");
        assert_eq!(identity.email.as_deref(), Some("person@example.com"));
    }

    #[test]
    fn maps_invalid_grant_to_rejected_credentials() {
        let error =
            map_refresh_error(StatusCode::BAD_REQUEST, Some("invalid_grant"), None).unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::Unauthorized);
        assert!(error.short_message().contains("invalid_grant"));
    }
}
