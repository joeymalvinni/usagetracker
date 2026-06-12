use std::path::PathBuf;

use chrono::Utc;
use keyring::{Entry, Error as KeyringError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::providers::{ProviderError, ProviderErrorKind};

pub(super) const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

const TOKEN_REFRESH_SKEW_MS: i64 = 60_000;

pub(super) async fn load_credentials(
    keychain_account: String,
    credentials_file_path: PathBuf,
) -> Result<ClaudeCredentials, ProviderError> {
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

pub(super) async fn save_credentials(credentials: ClaudeCredentials) -> Result<(), ProviderError> {
    match credentials.source.clone() {
        CredentialSource::Keychain => {
            let keychain_account = credentials.keychain_account.clone();
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

pub(super) fn parse_credentials(
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
pub(super) enum CredentialSource {
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
pub(super) struct TokenRefreshResponse {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "refresh_token")]
    pub refresh_token: Option<String>,
    #[serde(rename = "expires_in")]
    pub expires_in: i64,
    #[serde(rename = "token_type")]
    pub token_type: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ClaudeCredentials {
    pub keychain_account: String,
    source: CredentialSource,
    pub access_token: String,
    pub refresh_token: String,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub scopes: Vec<String>,
    raw: Value,
}

impl ClaudeCredentials {
    pub(super) fn account_id(&self) -> String {
        self.keychain_account.clone()
    }

    pub(super) fn display_name(&self) -> String {
        match self.subscription_type.as_deref() {
            Some(subscription_type) if !subscription_type.trim().is_empty() => {
                format!("Claude {subscription_type}")
            }
            _ => "Claude".to_string(),
        }
    }

    pub(super) fn is_expired(&self) -> bool {
        self.expires_at_ms.is_some_and(|expires_at| {
            expires_at <= Utc::now().timestamp_millis() + TOKEN_REFRESH_SKEW_MS
        })
    }

    pub(super) fn with_refreshed_tokens(
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
}
