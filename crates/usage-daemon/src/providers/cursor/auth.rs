use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::Utc;
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::Deserialize;

use crate::providers::{browser_cookies, ProviderError, ProviderErrorKind};

const ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
const COOKIE_ENV: &str = "USAGE_TRACKER_CURSOR_COOKIE";
pub(super) const COOKIE_NAMES: [&str; 7] = [
    "WorkosCursorSessionToken",
    "__Secure-next-auth.session-token",
    "next-auth.session-token",
    "wos-session",
    "__Secure-wos-session",
    "authjs.session-token",
    "__Secure-authjs.session-token",
];
// Requests are sent only to cursor.com, so do not forward cookies scoped to
// Cursor's other hosts even when they use a familiar session-cookie name.
const COOKIE_DOMAINS: [&str; 1] = ["cursor.com"];

#[derive(Clone)]
pub(super) struct SessionCredential {
    pub(super) cookie_header: String,
    pub(super) source: SessionSource,
    pub(super) account_hint: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionSource {
    Manual,
    CursorApp,
    Browser,
}

impl SessionSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::CursorApp => "cursor_app",
            Self::Browser => "browser",
        }
    }
}

pub(super) fn load_session_candidates() -> Result<Vec<SessionCredential>, ProviderError> {
    if let Ok(raw) = std::env::var(COOKIE_ENV) {
        let cookie_header = normalize_cookie_header(&raw).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                format!("{COOKIE_ENV} did not contain a recognized Cursor session cookie"),
            )
        })?;
        return Ok(vec![SessionCredential {
            cookie_header,
            source: SessionSource::Manual,
            account_hint: None,
        }]);
    }

    let mut candidates = Vec::new();
    if let Some(candidate) = load_cursor_app_session()? {
        candidates.push(candidate);
    }

    if std::env::var("USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT").as_deref() == Ok("1") {
        match browser_cookies::import_browser_cookie_sessions(
            &COOKIE_DOMAINS,
            &COOKIE_NAMES,
            Some(&COOKIE_NAMES),
        ) {
            Ok(sessions) => {
                candidates.extend(sessions.into_iter().map(|session| SessionCredential {
                    cookie_header: session.header,
                    source: SessionSource::Browser,
                    account_hint: None,
                }))
            }
            Err(error) if candidates.is_empty() => return Err(error),
            Err(_) => {}
        }
    }

    if candidates.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            "Cursor is not signed in locally; open Cursor or allow browser cookie import",
        ));
    }
    Ok(candidates)
}

pub(super) fn normalize_cookie_header(raw: &str) -> Option<String> {
    browser_cookies::normalize_cookie_header(raw, &COOKIE_NAMES, Some(&COOKIE_NAMES))
}

fn load_cursor_app_session() -> Result<Option<SessionCredential>, ProviderError> {
    let Some(path) = cursor_state_db_path() else {
        return Ok(None);
    };
    load_cursor_app_session_from(&path)
}

fn cursor_state_db_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        Some(base.join("Cursor/User/globalStorage/state.vscdb"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

pub(super) fn load_cursor_app_session_from(
    path: &Path,
) -> Result<Option<SessionCredential>, ProviderError> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("failed to open Cursor app state database: {err}"),
            )
        })?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(cursor_db_error)?;
    let mut statement = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1")
        .map_err(cursor_db_error)?;
    let mut rows = statement
        .query([ACCESS_TOKEN_KEY])
        .map_err(cursor_db_error)?;
    let Some(row) = rows.next().map_err(cursor_db_error)? else {
        return Ok(None);
    };
    let token = match row.get_ref(0).map_err(cursor_db_error)? {
        ValueRef::Text(value) | ValueRef::Blob(value) => String::from_utf8(value.to_vec()).ok(),
        _ => None,
    }
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty());
    let Some(token) = token else {
        return Ok(None);
    };
    let claims = jwt_claims(&token)?;
    let user_id = cursor_user_id(&claims.sub)?;
    if claims.exp <= Utc::now().timestamp() as f64 + 60.0 {
        return Ok(None);
    }
    Ok(Some(SessionCredential {
        cookie_header: format!("WorkosCursorSessionToken={user_id}%3A%3A{token}"),
        source: SessionSource::CursorApp,
        account_hint: Some(user_id),
    }))
}

fn cursor_db_error(error: rusqlite::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderUnavailable,
        format!("failed to read Cursor app authentication: {error}"),
    )
}

#[derive(Deserialize)]
struct JwtClaims {
    sub: String,
    exp: f64,
}

fn jwt_claims(token: &str) -> Result<JwtClaims, ProviderError> {
    let payload = token.split('.').nth(1).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Cursor app access token was not a JWT",
        )
    })?;
    let decoded = decode_base64url(payload).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Cursor app access token had an invalid payload",
        )
    })?;
    serde_json::from_slice(&decoded).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Cursor app access token was missing identity or expiration claims",
        )
    })
}

fn cursor_user_id(subject: &str) -> Result<String, ProviderError> {
    let value = subject
        .split('|')
        .filter(|part| !part.is_empty())
        .next_back()
        .unwrap_or_default();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Cursor app access token had an invalid user identity",
        ));
    }
    Ok(value.to_string())
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity((value.len() * 3).div_ceil(4));
    let mut buffer = 0_u32;
    let mut bits = 0_u8;
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
