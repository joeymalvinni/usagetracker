use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    keychain::{self, Error as KeychainError},
    providers::{ProviderError, ProviderErrorKind},
};

pub(super) const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

const TOKEN_REFRESH_SKEW_MS: i64 = 60_000;

type CredentialLocks = HashMap<(String, String), Weak<tokio::sync::Mutex<()>>>;
static CREDENTIAL_LOCKS: LazyLock<Mutex<CredentialLocks>> = LazyLock::new(Mutex::default);

/// Serializes refresh and file-to-Keychain migration for one credential item.
/// Weak entries keep deleted profiles from accumulating in the registry.
pub(super) async fn lock_credentials(
    service: &str,
    account: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = CREDENTIAL_LOCKS
            .lock()
            .expect("credential lock registry poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (service.to_string(), account.to_string());
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

/// A collector may have cached file credentials before launch seeded Keychain.
/// Re-evaluate source priority while holding the shared credential lock.
pub(super) async fn reload_for_refresh(
    credentials: &ClaudeCredentials,
) -> Result<ClaudeCredentials, ProviderError> {
    let service = credentials.keychain_service.clone();
    let account = credentials.keychain_account.clone();
    let source = credentials.source.clone();
    tokio::task::spawn_blocking(move || {
        keychain::revalidate_password(&service, &account).map_err(keychain_save_error)?;
        match source {
            CredentialSource::Keychain => load_keychain_credentials(&service, &account),
            CredentialSource::File(path) => {
                load_credentials_from_keychain_or_file(&service, &account, path)
            }
        }
    })
    .await
    .map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude credential reload task failed",
        )
    })?
}

pub(super) async fn load_credentials(
    keychain_service: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
) -> Result<ClaudeCredentials, ProviderError> {
    tokio::task::spawn_blocking(move || {
        load_credentials_from_keychain_or_file(
            &keychain_service,
            &keychain_account,
            credentials_file_path,
        )
    })
    .await
    .map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude credential load task failed",
        )
    })?
}

pub(super) async fn save_credentials(
    mut credentials: ClaudeCredentials,
    expected_contents: String,
) -> Result<ClaudeCredentials, ProviderError> {
    match credentials.source.clone() {
        CredentialSource::Keychain => {
            let keychain_service = credentials.keychain_service.clone();
            let keychain_account = credentials.keychain_account.clone();
            let contents = credentials.raw.to_string();
            let persisted_contents = contents.clone();
            tokio::task::spawn_blocking(move || {
                save_keychain_credentials(
                    &keychain_service,
                    &keychain_account,
                    &expected_contents,
                    &contents,
                )
            })
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "Claude credential save task failed",
                )
            })??;
            credentials.source_contents = persisted_contents;
        }
        CredentialSource::File(path) => {
            let contents = serde_json::to_vec_pretty(&credentials.raw).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "failed to serialize refreshed Claude credentials",
                )
            })?;
            let persisted_contents = format!("{}\n", String::from_utf8_lossy(&contents));
            tokio::task::spawn_blocking(move || {
                save_file_credentials(&path, &expected_contents, &contents)
            })
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "Claude credential file save task failed",
                )
            })??;
            credentials.source_contents = persisted_contents;
        }
    }
    Ok(credentials)
}

/// Ensures Claude Code can read the selected profile's credentials. The refresh
/// callback persists rotated tokens with the normal compare-and-set safeguards.
pub(super) async fn sync_for_launch<F, Fut>(
    keychain_service: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
    refresh: F,
) -> Result<(), ProviderError>
where
    F: Fn(ClaudeCredentials) -> Fut,
    Fut: std::future::Future<Output = Result<ClaudeCredentials, ProviderError>>,
{
    let _guard = lock_credentials(&keychain_service, &keychain_account).await;
    let load = || {
        let service = keychain_service.clone();
        let account = keychain_account.clone();
        let path = credentials_file_path.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                // Check for external rotations while retaining an Allow once
                // value if macOS refuses a new silent read.
                keychain::revalidate_password(&service, &account).map_err(keychain_save_error)?;
                load_credentials_from_keychain_or_file(&service, &account, path)
            })
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::CredentialsInvalid,
                    "Claude credential load task failed",
                )
            })?
        }
    };
    let seed = |contents: String| {
        let service = keychain_service.clone();
        let account = keychain_account.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                keychain::create_password_if_missing(&service, &account, &contents)
            })
            .await
            .map_err(|_| KeychainError::HelperUnavailable)?
        }
    };
    sync_for_launch_with(load, refresh, seed).await
}

async fn sync_for_launch_with<L, LF, R, RF, S, SF>(
    load: L,
    refresh: R,
    seed: S,
) -> Result<(), ProviderError>
where
    L: Fn() -> LF,
    LF: std::future::Future<Output = Result<ClaudeCredentials, ProviderError>>,
    R: Fn(ClaudeCredentials) -> RF,
    RF: std::future::Future<Output = Result<ClaudeCredentials, ProviderError>>,
    S: Fn(String) -> SF,
    SF: std::future::Future<Output = Result<(), KeychainError>>,
{
    for attempt in 0..3 {
        let mut credentials = load().await?;
        if credentials.is_expired() {
            credentials = match refresh(credentials).await {
                Ok(credentials) => credentials,
                // A guarded refresh may lose to another writer. Re-read the
                // source before retrying; never write our stale snapshot back.
                Err(error)
                    if error.kind() == ProviderErrorKind::CredentialsInvalid && attempt < 2 =>
                {
                    continue
                }
                Err(error) => return Err(error),
            };
        }
        if matches!(credentials.source, CredentialSource::Keychain) {
            // Already in the right place. Refresh, if needed, persisted it.
            // Rewriting here could undo a concurrent refresh or reconnect.
            return Ok(());
        }
        match seed(credentials.raw.to_string()).await {
            Ok(()) => return Ok(()),
            Err(KeychainError::Conflict) => continue,
            Err(error) => return Err(keychain_save_error(error)),
        }
    }
    Err(credential_conflict())
}

fn save_file_credentials(
    path: &Path,
    expected_contents: &str,
    contents: &[u8],
) -> Result<(), ProviderError> {
    match std::fs::read_to_string(path) {
        Ok(current) if current == expected_contents => {}
        _ => return Err(credential_conflict()),
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".credentials.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Ok(directory) = std::fs::File::open(parent) {
            directory.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "failed to atomically save refreshed Claude credentials file",
        )
    })
}

fn load_credentials_from_keychain_or_file(
    keychain_service: &str,
    keychain_account: &str,
    credentials_file_path: PathBuf,
) -> Result<ClaudeCredentials, ProviderError> {
    credentials_with_file_fallback(
        load_keychain_credentials(keychain_service, keychain_account),
        || load_file_credentials(&credentials_file_path, keychain_service, keychain_account),
    )
}

fn credentials_with_file_fallback(
    primary: Result<ClaudeCredentials, ProviderError>,
    load_file: impl FnOnce() -> Result<ClaudeCredentials, ProviderError>,
) -> Result<ClaudeCredentials, ProviderError> {
    match primary {
        Ok(credentials) => Ok(credentials),
        Err(error)
            if matches!(
                error.kind(),
                ProviderErrorKind::CredentialsMissing | ProviderErrorKind::KeychainAccessFailed
            ) =>
        {
            match load_file() {
                Ok(credentials) => Ok(credentials),
                Err(file_error) if file_error.kind() == ProviderErrorKind::CredentialsMissing => {
                    Err(error)
                }
                Err(file_error) => Err(file_error),
            }
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn request_credential_access(
    service: String,
    account: String,
) -> Result<(), ProviderError> {
    tokio::task::spawn_blocking(move || {
        keychain::request_access(&service, &account).map_err(keychain_load_error)
    })
    .await
    .map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::KeychainAccessFailed,
            "Credential access request could not complete",
        )
    })?
}

fn load_keychain_credentials(
    keychain_service: &str,
    keychain_account: &str,
) -> Result<ClaudeCredentials, ProviderError> {
    let password =
        keychain::get_password(keychain_service, keychain_account).map_err(keychain_load_error)?;

    parse_credentials(
        &password,
        keychain_service,
        keychain_account,
        CredentialSource::Keychain,
    )
}

fn keychain_load_error(error: KeychainError) -> ProviderError {
    match error {
        KeychainError::Missing => ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            "Claude Code credentials are missing from macOS Keychain",
        ),
        KeychainError::InteractionRequired => ProviderError::new(
            ProviderErrorKind::KeychainAccessFailed,
            "macOS credential access needs permission. Choose Allow access to continue.",
        ),
        KeychainError::AuthenticationFailed => ProviderError::new(
            ProviderErrorKind::KeychainAccessFailed,
            "macOS did not authorize credential access",
        ),
        _ => ProviderError::new(
            ProviderErrorKind::KeychainAccessFailed,
            "failed to access Claude Code credentials in macOS Keychain",
        ),
    }
}

fn save_keychain_credentials(
    keychain_service: &str,
    keychain_account: &str,
    expected_contents: &str,
    contents: &str,
) -> Result<(), ProviderError> {
    keychain::compare_and_set_password(
        keychain_service,
        keychain_account,
        expected_contents,
        contents,
    )
    .map_err(|error| {
        if error == KeychainError::Conflict {
            credential_conflict()
        } else {
            keychain_save_error(error)
        }
    })
}

fn keychain_save_error(error: KeychainError) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::KeychainAccessFailed,
        if error == KeychainError::AuthenticationFailed {
            "macOS did not authorize credential access"
        } else {
            "failed to save Claude Code credentials in macOS Keychain"
        },
    )
}

fn credential_conflict() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::CredentialsInvalid,
        "Claude credentials changed during token refresh; reloading before retry",
    )
}

fn load_file_credentials(
    path: &PathBuf,
    keychain_service: &str,
    keychain_account: &str,
) -> Result<ClaudeCredentials, ProviderError> {
    let contents = std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ProviderError::new(
                ProviderErrorKind::CredentialsMissing,
                format!(
                    "Claude credentials are missing from Keychain and {}",
                    path.display()
                ),
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
        keychain_service,
        keychain_account,
        CredentialSource::File(path.clone()),
    )
}

pub(super) fn parse_credentials(
    contents: &str,
    keychain_service: impl Into<String>,
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
        keychain_service: keychain_service.into(),
        keychain_account: keychain_account.into(),
        source,
        access_token: oauth.access_token,
        refresh_token: oauth.refresh_token,
        subscription_type: oauth.subscription_type,
        rate_limit_tier: oauth.rate_limit_tier,
        expires_at_ms: oauth.expires_at,
        scopes: oauth.scopes,
        raw,
        source_contents: contents.to_string(),
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
    pub keychain_service: String,
    pub keychain_account: String,
    source: CredentialSource,
    pub access_token: String,
    pub refresh_token: String,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub scopes: Vec<String>,
    raw: Value,
    source_contents: String,
}

impl ClaudeCredentials {
    pub(super) fn source_contents(&self) -> &str {
        &self.source_contents
    }
    pub(super) fn source_label(&self) -> &'static str {
        match self.source {
            CredentialSource::Keychain => "keychain",
            CredentialSource::File(_) => "file",
        }
    }

    #[cfg(test)]
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn permission_failure_can_use_the_selected_profiles_file() {
        let result = credentials_with_file_fallback(
            Err(keychain_load_error(KeychainError::InteractionRequired)),
            || {
                parse_credentials(
                    r#"{"claudeAiOauth":{"accessToken":"file-access","refreshToken":"refresh"}}"#,
                    "profile-service",
                    "profile-account",
                    CredentialSource::File(PathBuf::from("/profiles/work/.credentials.json")),
                )
            },
        )
        .unwrap();
        assert_eq!(result.access_token, "file-access");
        assert_eq!(result.source_label(), "file");
    }

    #[test]
    fn missing_fallback_preserves_permission_error_and_invalid_keychain_does_not_switch_sources() {
        let error = credentials_with_file_fallback(
            Err(keychain_load_error(KeychainError::InteractionRequired)),
            || {
                Err(ProviderError::new(
                    ProviderErrorKind::CredentialsMissing,
                    "no file",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::KeychainAccessFailed);
        let error = credentials_with_file_fallback(
            Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "invalid JSON",
            )),
            || panic!("malformed credentials must not silently select another account"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::CredentialsInvalid);
    }

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
            CLAUDE_KEYCHAIN_SERVICE,
            "joey",
            CredentialSource::Keychain,
        )
        .unwrap();

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
            CLAUDE_KEYCHAIN_SERVICE,
            "joey",
            CredentialSource::Keychain,
        )
        .unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::CredentialsInvalid);
    }

    #[test]
    fn distinguishes_keychain_access_from_invalid_claude_credentials() {
        let authentication = keychain_load_error(KeychainError::AuthenticationFailed);
        let backend = keychain_load_error(KeychainError::BackendRejected);
        let missing = keychain_load_error(KeychainError::Missing);

        assert_eq!(
            authentication.kind(),
            ProviderErrorKind::KeychainAccessFailed
        );
        assert_eq!(backend.kind(), ProviderErrorKind::KeychainAccessFailed);
        assert_eq!(missing.kind(), ProviderErrorKind::CredentialsMissing);
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
            CLAUDE_KEYCHAIN_SERVICE,
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
    fn saves_file_credentials_atomically_and_privately() {
        let root =
            std::env::temp_dir().join(format!("claude-credentials-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".credentials.json");
        std::fs::write(&path, "old").unwrap();

        save_file_credentials(&path, "old", br#"{"token":"secret"}"#).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"token\":\"secret\"}\n"
        );
        assert!(save_file_credentials(&path, "old", br#"{"token":"stale"}"#).is_err());
        save_file_credentials(&path, "{\"token\":\"secret\"}\n", br#"{"token":"new"}"#).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_credentials_are_detected_with_skew() {
        let mut credentials = parse_credentials(
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#,
            "svc",
            "acct",
            CredentialSource::Keychain,
        )
        .unwrap();
        assert!(credentials.is_expired());
        credentials = parse_credentials(
            &format!(
                r#"{{"claudeAiOauth":{{"accessToken":"a","refreshToken":"r","expiresAt":{}}}}}"#,
                chrono::Utc::now().timestamp_millis() + 3_600_000
            ),
            "svc",
            "acct",
            CredentialSource::Keychain,
        )
        .unwrap();
        assert!(!credentials.is_expired());
    }

    fn valid_oauth_json(access_token: &str, refresh_token: &str, expires_at_ms: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{access_token}","refreshToken":"{refresh_token}","expiresAt":{expires_at_ms},"scopes":[]}}}}"#
        )
    }

    mod sync_for_launch_tests {
        use super::*;
        use std::{
            cell::{Cell, RefCell},
            future::ready,
        };

        fn credentials(access: &str, source: CredentialSource, expired: bool) -> ClaudeCredentials {
            let expires_at = if expired {
                1
            } else {
                Utc::now().timestamp_millis() + 3_600_000
            };
            parse_credentials(
                &valid_oauth_json(access, "refresh", expires_at),
                "svc",
                "acct",
                source,
            )
            .unwrap()
        }

        #[tokio::test]
        async fn credential_lock_serializes_same_item_but_not_other_accounts() {
            let service = uuid::Uuid::new_v4().to_string();
            let first = lock_credentials(&service, "account").await;
            let mut competing = Box::pin(lock_credentials(&service, "account"));
            assert!(futures_util::poll!(&mut competing).is_pending());
            let mut other_account = Box::pin(lock_credentials(&service, "other"));
            assert!(futures_util::poll!(&mut other_account).is_ready());
            drop(first);
            assert!(futures_util::poll!(&mut competing).is_ready());
        }

        #[tokio::test]
        async fn launch_does_not_overwrite_a_concurrent_keychain_refresh() {
            let stored = RefCell::new("A");
            sync_for_launch_with(
                || {
                    let loaded = credentials(&stored.borrow(), CredentialSource::Keychain, false);
                    // Polling writes B after launch has read A.
                    *stored.borrow_mut() = "B";
                    ready(Ok(loaded))
                },
                |_| async { panic!("unexpired credentials must not be refreshed") },
                |_| async { panic!("existing Keychain credentials must never be re-seeded") },
            )
            .await
            .unwrap();
            assert_eq!(*stored.borrow(), "B");
        }

        #[tokio::test]
        async fn launch_reloads_after_a_guarded_refresh_conflict() {
            let loads = Cell::new(0);
            let refreshes = Cell::new(0);
            sync_for_launch_with(
                || {
                    let first = loads.get() == 0;
                    loads.set(loads.get() + 1);
                    ready(Ok(credentials(
                        if first { "A" } else { "B" },
                        CredentialSource::Keychain,
                        first,
                    )))
                },
                |_| {
                    refreshes.set(refreshes.get() + 1);
                    ready(Err(credential_conflict()))
                },
                |_| async { panic!("must preserve the winner of the guarded refresh") },
            )
            .await
            .unwrap();
            assert_eq!(loads.get(), 2);
            assert_eq!(refreshes.get(), 1);
        }

        #[tokio::test]
        async fn launch_seeds_a_missing_keychain_item_from_file() {
            let stored = RefCell::new(None);
            sync_for_launch_with(
                || {
                    ready(Ok(credentials(
                        "file",
                        CredentialSource::File(PathBuf::from("unused")),
                        false,
                    )))
                },
                |_| async { panic!("valid file credentials need no refresh") },
                |contents| {
                    *stored.borrow_mut() = Some(contents);
                    ready(Ok(()))
                },
            )
            .await
            .unwrap();
            let raw: Value = serde_json::from_str(stored.borrow().as_ref().unwrap()).unwrap();
            assert_eq!(raw["claudeAiOauth"]["accessToken"], "file");
        }

        #[tokio::test]
        async fn launch_reloads_keychain_if_another_writer_wins_file_seeding() {
            let loads = Cell::new(0);
            let seeds = Cell::new(0);
            sync_for_launch_with(
                || {
                    let first = loads.get() == 0;
                    loads.set(loads.get() + 1);
                    ready(Ok(credentials(
                        if first { "file-A" } else { "keychain-B" },
                        if first {
                            CredentialSource::File(PathBuf::from("unused"))
                        } else {
                            CredentialSource::Keychain
                        },
                        false,
                    )))
                },
                |_| async { panic!("valid credentials need no refresh") },
                |_| {
                    seeds.set(seeds.get() + 1);
                    ready(Err(KeychainError::Conflict))
                },
            )
            .await
            .unwrap();
            assert_eq!(loads.get(), 2);
            assert_eq!(seeds.get(), 1);
        }

        #[tokio::test]
        async fn launch_seeds_refreshed_file_tokens_and_bounds_conflict_retries() {
            let writes = Cell::new(0);
            let error = sync_for_launch_with(
                || {
                    ready(Ok(credentials(
                        "expired",
                        CredentialSource::File(PathBuf::from("unused")),
                        true,
                    )))
                },
                |_| {
                    ready(Ok(credentials(
                        "refreshed",
                        CredentialSource::File(PathBuf::from("unused")),
                        false,
                    )))
                },
                |contents| {
                    let raw: Value = serde_json::from_str(&contents).unwrap();
                    assert_eq!(raw["claudeAiOauth"]["accessToken"], "refreshed");
                    writes.set(writes.get() + 1);
                    ready(Err(KeychainError::Conflict))
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ProviderErrorKind::CredentialsInvalid);
            assert_eq!(writes.get(), 3);
        }

        #[tokio::test]
        async fn launch_stops_on_keychain_and_refresh_failures() {
            let error = sync_for_launch_with(
                || {
                    ready(Err(ProviderError::new(
                        ProviderErrorKind::KeychainAccessFailed,
                        "denied",
                    )))
                },
                |_| async { panic!("must not refresh after a Keychain error") },
                |_| async { panic!("must not seed after a Keychain error") },
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ProviderErrorKind::KeychainAccessFailed);
            for kind in [
                ProviderErrorKind::Network,
                ProviderErrorKind::RateLimited,
                ProviderErrorKind::Unauthorized,
            ] {
                let loads = Cell::new(0);
                let error = sync_for_launch_with(
                    || {
                        loads.set(loads.get() + 1);
                        ready(Ok(credentials("old", CredentialSource::Keychain, true)))
                    },
                    |_| ready(Err(ProviderError::new(kind, "refresh failed"))),
                    |_| async { panic!("must not seed after failed refresh") },
                )
                .await
                .unwrap_err();
                assert_eq!(error.kind(), kind);
                assert_eq!(loads.get(), 1);
            }
        }
    }
}
