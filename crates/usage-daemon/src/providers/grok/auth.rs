use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::providers::{ProviderError, ProviderErrorKind};

const OIDC_SCOPE_PREFIX: &str = "https://auth.x.ai::";

#[derive(Clone, Debug)]
pub(super) struct GrokCredentials {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) scope: String,
    pub(super) user_id: Option<String>,
    pub(super) team_id: Option<String>,
    pub(super) email: Option<String>,
    pub(super) display_name: Option<String>,
    pub(super) first_name: Option<String>,
    pub(super) last_name: Option<String>,
    pub(super) oidc_issuer: Option<String>,
    pub(super) oidc_client_id: Option<String>,
    pub(super) expires_at: Option<DateTime<Utc>>,
    pub(super) created_at: Option<DateTime<Utc>>,
    pub(super) login_method: &'static str,
}

impl GrokCredentials {
    pub(super) fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|expires| expires <= Utc::now())
    }

    pub(super) fn external_account_id(&self) -> String {
        self.user_id
            .clone()
            .or_else(|| self.email.clone())
            .unwrap_or_else(|| super::DEFAULT_EXTERNAL_ACCOUNT_ID.to_string())
    }

    /// Identity diagnostics intentionally exclude access and refresh tokens.
    pub(super) fn metadata(&self) -> Value {
        json!({
            "email": self.email,
            "team_id": self.team_id,
            "user_id": self.user_id,
            "first_name": self.first_name,
            "last_name": self.last_name,
            "login_method": self.login_method,
            "auth_scope": self.scope,
            "oidc_issuer": self.oidc_issuer,
            "oidc_client_id": self.oidc_client_id,
            "expires_at": self.expires_at,
            "created_at": self.created_at,
            "expired": self.is_expired(),
            "refresh_token_available": self.refresh_token.is_some(),
        })
    }
}

pub(super) fn grok_home() -> Result<PathBuf, ProviderError> {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "failed to resolve GROK_HOME",
            )
        })
}

pub(super) fn auth_path() -> Result<PathBuf, ProviderError> {
    Ok(grok_home()?.join("auth.json"))
}

pub(super) fn auth_file_exists() -> bool {
    auth_path().is_ok_and(|path| path.exists())
}

pub(super) fn load_credentials() -> Result<GrokCredentials, ProviderError> {
    let path = auth_path()?;
    let data = std::fs::read(&path).map_err(|err| {
        ProviderError::new(
            if err.kind() == std::io::ErrorKind::NotFound {
                ProviderErrorKind::CredentialsMissing
            } else {
                ProviderErrorKind::ProviderUnavailable
            },
            if err.kind() == std::io::ErrorKind::NotFound {
                "Grok credentials were not found; run `grok login`".to_string()
            } else {
                format!("could not read Grok auth.json: {err}")
            },
        )
    })?;
    parse_credentials(&data)
}

pub(super) fn parse_credentials(data: &[u8]) -> Result<GrokCredentials, ProviderError> {
    let root: Value = serde_json::from_slice(data).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            format!("Grok auth.json is invalid JSON: {err}"),
        )
    })?;
    let object = root.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Grok auth.json must contain an object",
        )
    })?;
    let mut candidates = object.iter().filter_map(candidate).collect::<Vec<_>>();
    candidates.sort_by(|lhs, rhs| (lhs.0, lhs.1).cmp(&(rhs.0, rhs.1)));
    let (_, scope, entry, access_token) = candidates.into_iter().next().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Grok auth.json contains no usable access token; run `grok login`",
        )
    })?;
    let first = text(entry.get("first_name"));
    let last = text(entry.get("last_name"));
    let display_name = [first.as_deref(), last.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<&str>>()
        .join(" ");
    Ok(GrokCredentials {
        access_token,
        refresh_token: text_any(entry, &["refresh_token", "refreshToken"]),
        scope: scope.clone(),
        user_id: text_any(entry, &["user_id", "userId"]),
        team_id: text_any(entry, &["team_id", "teamId"]),
        email: text(entry.get("email")),
        display_name: (!display_name.is_empty()).then_some(display_name),
        first_name: first,
        last_name: last,
        oidc_issuer: text_any(entry, &["oidc_issuer", "oidcIssuer", "issuer"]),
        oidc_client_id: text_any(
            entry,
            &["oidc_client_id", "oidcClientId", "client_id", "clientId"],
        ),
        expires_at: value_any(entry, &["expires_at", "expiresAt"]).and_then(timestamp),
        created_at: value_any(
            entry,
            &["created_at", "createdAt", "create_time", "createTime"],
        )
        .and_then(timestamp),
        login_method: if scope.starts_with(OIDC_SCOPE_PREFIX) {
            "SuperGrok"
        } else {
            "session"
        },
    })
}

type Candidate<'a> = (u8, &'a String, &'a Map<String, Value>, String);

fn candidate<'a>((scope, value): (&'a String, &'a Value)) -> Option<Candidate<'a>> {
    let entry = value.as_object()?;
    // A partially-written OIDC record must not shadow a complete legacy
    // session. `text` rejects both absent and whitespace-only keys.
    let token = text(entry.get("key"))?;
    let rank = if scope.starts_with(OIDC_SCOPE_PREFIX) {
        0
    } else if scope == "https://accounts.x.ai/sign-in" || scope.contains("/sign-in") {
        1
    } else {
        return None;
    };
    Some((rank, scope, entry, token))
}

fn value_any<'a>(entry: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| entry.get(*key))
}

fn text_any(entry: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    text(value_any(entry, keys))
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(value) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Ok(raw) = value.parse::<i64>() {
            return unix_timestamp(raw);
        }
        return None;
    }
    value.as_i64().and_then(unix_timestamp)
}

fn unix_timestamp(raw: i64) -> Option<DateTime<Utc>> {
    let seconds = if raw > 10_000_000_000 {
        raw / 1_000
    } else {
        raw
    };
    DateTime::from_timestamp(seconds, 0)
}

fn text(value: Option<&Value>) -> Option<String> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prefers_oidc_credentials_and_never_requires_optional_identity() {
        let value = json!({
            "https://accounts.x.ai/sign-in":{"key":"legacy","email":"old@example.com"},
            "https://auth.x.ai::client":{"key":"oidc","user_id":"user-1","first_name":"Ada","last_name":"Lovelace"}
        });
        let credentials = parse_credentials(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(credentials.access_token, "oidc");
        assert_eq!(credentials.external_account_id(), "user-1");
        assert_eq!(credentials.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(credentials.login_method, "SuperGrok");
    }

    #[test]
    fn incomplete_oidc_entry_does_not_shadow_legacy_credentials() {
        let value = json!({
            "https://auth.x.ai::client":{"key":"  ","email":"new@example.com"},
            "https://accounts.x.ai/sign-in":{"key":"legacy","email":"old@example.com"}
        });
        let credentials = parse_credentials(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(credentials.access_token, "legacy");
        assert_eq!(credentials.login_method, "session");
    }

    #[test]
    fn parses_complete_identity_and_numeric_expiry_without_exposing_tokens() {
        let value = json!({
            "https://auth.x.ai::client": {
                "key":"secret-access", "refresh_token":"secret-refresh",
                "team_id":"team-1", "user_id":"user-1", "email":"ada@example.com",
                "oidc_issuer":"https://auth.x.ai", "oidc_client_id":"client",
                "expires_at":1800000000000_i64, "created_at":"2026-07-01T00:00:00Z"
            }
        });
        let credentials = parse_credentials(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(credentials.refresh_token.as_deref(), Some("secret-refresh"));
        assert_eq!(credentials.expires_at.unwrap().timestamp(), 1_800_000_000);
        let metadata = credentials.metadata().to_string();
        assert!(!metadata.contains("secret-access"));
        assert!(!metadata.contains("secret-refresh"));
        assert!(metadata.contains("team-1"));
    }

    #[test]
    fn rejects_invalid_shapes_and_entries_without_tokens() {
        assert_eq!(
            parse_credentials(b"[]").unwrap_err().kind(),
            ProviderErrorKind::CredentialsInvalid
        );
        let value = json!({"https://auth.x.ai::client":{"email":"ada@example.com"}});
        assert_eq!(
            parse_credentials(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .kind(),
            ProviderErrorKind::CredentialsInvalid
        );
    }
}
