use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::providers::{read_response_body, ProviderError, ProviderErrorKind};

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

        map_refresh_error(response.status())?;
        let body = read_response_body(response, "Claude token refresh response").await?;
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

        let refreshed = credentials.with_refreshed_tokens(refresh)?;
        save_credentials(refreshed.clone()).await?;
        Ok(refreshed)
    }

    pub(super) async fn fetch_usage(
        &self,
        credentials: &ClaudeCredentials,
    ) -> Result<Value, ProviderError> {
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

        map_usage_error(response.status())?;
        let body = read_response_body(response, "Claude usage response").await?;
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

        map_profile_error(response.status())?;
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

fn map_refresh_error(status: StatusCode) -> Result<(), ProviderError> {
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
    Ok(())
}

fn map_usage_error(status: StatusCode) -> Result<(), ProviderError> {
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
    Ok(())
}

fn map_profile_error(status: StatusCode) -> Result<(), ProviderError> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Claude OAuth profile credentials were rejected",
        ));
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
}
