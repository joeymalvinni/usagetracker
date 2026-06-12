use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;

use crate::providers::{ProviderError, ProviderErrorKind};

use super::credentials::{save_credentials, ClaudeCredentials, TokenRefreshResponse};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Clone)]
pub(super) struct ClaudeApiClient {
    client: reqwest::Client,
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
        let refresh: TokenRefreshResponse = response.json().await.map_err(|err| {
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
        response.json().await.map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Claude usage JSON was invalid: {err}"),
            )
        })
    }
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
