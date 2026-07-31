use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
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

fn sync_keychain_for_launch(
    keychain_service: &str,
    keychain_account: &str,
    contents: &str,
) -> Result<(), ProviderError> {
    #[cfg(test)]
    if let Some(hook) = test_keychain_sync_hook() {
        return hook(keychain_service, keychain_account, contents);
    }

    keychain::set_password_if_changed(keychain_service, keychain_account, contents)
        .map_err(keychain_save_error)
}

fn invalidate_keychain_cache_for_launch(
    keychain_service: &str,
    keychain_account: &str,
) -> Result<(), ProviderError> {
    #[cfg(test)]
    if let Some(hook) = test_keychain_invalidate_hook() {
        return hook(keychain_service, keychain_account);
    }

    keychain::invalidate_password_cache(keychain_service, keychain_account)
        .map_err(keychain_save_error)
}

#[cfg(test)]
fn test_keychain_sync_hook(
) -> Option<
    fn(
        &str,
        &str,
        &str,
    ) -> Result<(), ProviderError>,
> {
    TEST_KEYCHAIN_SYNC_FN.with(|hook| *hook.borrow())
}

#[cfg(test)]
fn test_keychain_invalidate_hook() -> Option<fn(&str, &str) -> Result<(), ProviderError>> {
    TEST_KEYCHAIN_INVALIDATE_FN.with(|hook| *hook.borrow())
}

#[cfg(test)]
thread_local! {
    static TEST_KEYCHAIN_SYNC_FN: std::cell::RefCell<
        Option<fn(&str, &str, &str) -> Result<(), ProviderError>>,
    > = const { std::cell::RefCell::new(None) };
    static TEST_KEYCHAIN_INVALIDATE_FN: std::cell::RefCell<
        Option<fn(&str, &str) -> Result<(), ProviderError>>,
    > = const { std::cell::RefCell::new(None) };
    static TEST_KEYCHAIN_WRITES: std::cell::RefCell<Vec<(String, String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static TEST_KEYCHAIN_INVALIDATED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
}

#[cfg(test)]
fn test_record_keychain_sync(
    service: &str,
    account: &str,
    contents: &str,
) -> Result<(), ProviderError> {
    TEST_KEYCHAIN_WRITES.with(|writes| {
        writes.borrow_mut().push((
            service.to_string(),
            account.to_string(),
            contents.to_string(),
        ));
    });
    Ok(())
}

#[cfg(test)]
fn test_record_keychain_invalidate(_service: &str, _account: &str) -> Result<(), ProviderError> {
    TEST_KEYCHAIN_INVALIDATED.with(|flag| *flag.borrow_mut() = true);
    Ok(())
}

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
static TEST_KEYCHAIN_LOAD_MISSING: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn test_keychain_load_missing_enabled() -> bool {
    TEST_KEYCHAIN_LOAD_MISSING.load(Ordering::SeqCst)
}

async fn sync_loaded_credentials_to_keychain<F, Fut>(
    keychain_service: String,
    keychain_account: String,
    mut credentials: ClaudeCredentials,
    refresh: F,
) -> Result<(), ProviderError>
where
    F: FnOnce(ClaudeCredentials) -> Fut,
    Fut: std::future::Future<Output = Result<ClaudeCredentials, ProviderError>>,
{
    if credentials.is_expired() {
        credentials = refresh(credentials).await?;
    }

    let contents = credentials.raw.to_string();
    #[cfg(test)]
    if test_keychain_sync_hook().is_some() {
        sync_keychain_for_launch(&keychain_service, &keychain_account, &contents)?;
        invalidate_keychain_cache_for_launch(&keychain_service, &keychain_account)?;
        return Ok(());
    }

    let service = keychain_service.clone();
    let account = keychain_account.clone();
    tokio::task::spawn_blocking(move || {
        sync_keychain_for_launch(&service, &account, &contents)
    })
    .await
    .map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude credential sync task failed",
        )
    })??;

    tokio::task::spawn_blocking(move || {
        invalidate_keychain_cache_for_launch(&keychain_service, &keychain_account)
    })
    .await
    .map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "Claude credential cache invalidation task failed",
        )
    })??;

    Ok(())
}

pub(super) async fn sync_for_launch<F, Fut>(
    keychain_service: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
    refresh: F,
) -> Result<(), ProviderError>
where
    F: FnOnce(ClaudeCredentials) -> Fut,
    Fut: std::future::Future<Output = Result<ClaudeCredentials, ProviderError>>,
{
    let credentials = load_credentials(
        keychain_service.clone(),
        keychain_account.clone(),
        credentials_file_path,
    )
    .await?;

    sync_loaded_credentials_to_keychain(keychain_service, keychain_account, credentials, refresh)
        .await
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
    match load_keychain_credentials(keychain_service, keychain_account) {
        Ok(credentials) => Ok(credentials),
        Err(err) if err.kind() == ProviderErrorKind::CredentialsMissing => {
            load_file_credentials(&credentials_file_path, keychain_service, keychain_account)
        }
        Err(err) => Err(err),
    }
}

fn load_keychain_credentials(
    keychain_service: &str,
    keychain_account: &str,
) -> Result<ClaudeCredentials, ProviderError> {
    #[cfg(test)]
    if test_keychain_load_missing_enabled() {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            "Claude Code credentials are missing from macOS Keychain",
        ));
    }

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
        KeychainError::AuthenticationFailed => ProviderError::new(
            ProviderErrorKind::KeychainAccessFailed,
            "macOS Keychain authentication failed after 3 attempts",
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
            "macOS Keychain authentication failed after 3 attempts"
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

    struct TestKeychainHooksGuard {
        simulate_keychain_missing: bool,
    }

    impl TestKeychainHooksGuard {
        fn install_with_missing_keychain() -> Self {
            Self::install_with_options(true)
        }

        fn install_with_options(simulate_keychain_missing: bool) -> Self {
            if simulate_keychain_missing {
                TEST_KEYCHAIN_LOAD_MISSING.store(true, Ordering::SeqCst);
            }
            TEST_KEYCHAIN_WRITES.with(|writes| writes.borrow_mut().clear());
            TEST_KEYCHAIN_INVALIDATED.with(|flag| *flag.borrow_mut() = false);
            TEST_KEYCHAIN_SYNC_FN.with(|hook| *hook.borrow_mut() = Some(test_record_keychain_sync));
            TEST_KEYCHAIN_INVALIDATE_FN
                .with(|hook| *hook.borrow_mut() = Some(test_record_keychain_invalidate));
            Self {
                simulate_keychain_missing,
            }
        }
    }

    impl Drop for TestKeychainHooksGuard {
        fn drop(&mut self) {
            if self.simulate_keychain_missing {
                TEST_KEYCHAIN_LOAD_MISSING.store(false, Ordering::SeqCst);
            }
            TEST_KEYCHAIN_SYNC_FN.with(|hook| *hook.borrow_mut() = None);
            TEST_KEYCHAIN_INVALIDATE_FN.with(|hook| *hook.borrow_mut() = None);
        }
    }

    fn seed_temp_credentials_file(json: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("claude-sync-launch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".credentials.json");
        std::fs::write(&path, json).unwrap();
        (path, root)
    }

    fn test_service_account() -> (String, String) {
        (
            format!("usage-tracker-test-sync-{}", uuid::Uuid::new_v4()),
            "test-account".to_string(),
        )
    }

    mod sync_for_launch_tests {
        use super::*;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        fn parse_test_credentials(json: &str) -> ClaudeCredentials {
            parse_credentials(json, "svc", "acct", CredentialSource::Keychain).unwrap()
        }

        #[tokio::test]
        async fn sync_for_launch_seeds_keychain_from_credentials_file() {
            let expires_at = Utc::now().timestamp_millis() + 3_600_000;
            let json = valid_oauth_json("file-access", "file-refresh", expires_at);
            let (credentials_path, root) = seed_temp_credentials_file(&json);
            let (service, account) = test_service_account();
            let _guard = TestKeychainHooksGuard::install_with_missing_keychain();

            sync_for_launch(
                service.clone(),
                account.clone(),
                credentials_path,
                |_| async {
                    panic!("refresh should not be called for valid file-sourced credentials");
                    #[allow(unreachable_code)]
                    Ok(parse_test_credentials(""))
                },
            )
            .await
            .unwrap();

            let recorded = TEST_KEYCHAIN_WRITES.with(|writes| writes.borrow().clone());
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].0, service);
            assert_eq!(recorded[0].1, account);
            assert!(recorded[0].2.contains("\"accessToken\":\"file-access\""));
            assert!(recorded[0].2.contains("\"refreshToken\":\"file-refresh\""));
            assert!(TEST_KEYCHAIN_INVALIDATED.with(|flag| *flag.borrow()));
            std::fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test]
        async fn sync_for_launch_writes_valid_credentials_without_refresh() {
            let expires_at = Utc::now().timestamp_millis() + 3_600_000;
            let json = valid_oauth_json("access", "refresh", expires_at);
            let (credentials_path, root) = seed_temp_credentials_file(&json);
            let (service, account) = test_service_account();
            let _guard = TestKeychainHooksGuard::install_with_missing_keychain();

            sync_for_launch(
                service.clone(),
                account.clone(),
                credentials_path,
                |_| async {
                    panic!("refresh should not be called for valid credentials");
                    #[allow(unreachable_code)]
                    Ok(parse_test_credentials(""))
                },
            )
            .await
            .unwrap();

            let recorded = TEST_KEYCHAIN_WRITES.with(|writes| writes.borrow().clone());
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].0, service);
            assert_eq!(recorded[0].1, account);
            assert!(recorded[0].2.contains("\"accessToken\":\"access\""));
            assert!(recorded[0].2.contains("\"refreshToken\":\"refresh\""));
            assert!(TEST_KEYCHAIN_INVALIDATED.with(|flag| *flag.borrow()));
            std::fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test]
        async fn sync_for_launch_refreshes_expired_credentials_before_write() {
            let json = valid_oauth_json("old-access", "old-refresh", 1);
            let (credentials_path, root) = seed_temp_credentials_file(&json);
            let (service, account) = test_service_account();
            let refresh_called = Arc::new(AtomicBool::new(false));
            let refresh_called_clone = Arc::clone(&refresh_called);
            let _guard = TestKeychainHooksGuard::install_with_missing_keychain();

            sync_for_launch(
                service.clone(),
                account.clone(),
                credentials_path,
                move |credentials| {
                    let refresh_called = Arc::clone(&refresh_called_clone);
                    async move {
                        refresh_called.store(true, Ordering::SeqCst);
                        credentials.with_refreshed_tokens(TokenRefreshResponse {
                            access_token: "new-access".to_string(),
                            refresh_token: Some("new-refresh".to_string()),
                            expires_in: 3600,
                            token_type: None,
                        })
                    }
                },
            )
            .await
            .unwrap();

            assert!(refresh_called.load(Ordering::SeqCst));
            let recorded = TEST_KEYCHAIN_WRITES.with(|writes| writes.borrow().clone());
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].0, service);
            assert_eq!(recorded[0].1, account);
            assert!(recorded[0].2.contains("\"accessToken\":\"new-access\""));
            assert!(recorded[0].2.contains("\"refreshToken\":\"new-refresh\""));
            assert!(TEST_KEYCHAIN_INVALIDATED.with(|flag| *flag.borrow()));
            std::fs::remove_dir_all(root).unwrap();
        }

        #[cfg(target_os = "macos")]
        fn live_keychain_helper_available() -> bool {
            let service = format!("usage-tracker-test-sync-probe-{}", uuid::Uuid::new_v4());
            let available = keychain::set_password_if_changed(&service, "probe", "probe").is_ok();
            if available {
                let _ = keychain::delete_password(&service, "probe");
            }
            available
        }

        #[cfg(target_os = "macos")]
        fn cleanup_keychain(service: &str, account: &str) {
            let _ = keychain::delete_password(service, account);
        }

        #[cfg(target_os = "macos")]
        #[tokio::test]
        async fn sync_for_launch_round_trips_through_live_keychain() {
            if !live_keychain_helper_available() {
                eprintln!("skipping live Keychain sync test: helper unavailable in test binary");
                return;
            }

            let (service, account) = test_service_account();
            let expires_at = Utc::now().timestamp_millis() + 3_600_000;
            let json = valid_oauth_json("access", "refresh", expires_at);
            keychain::set_password_if_changed(&service, &account, &json).unwrap();

            sync_for_launch(
                service.clone(),
                account.clone(),
                PathBuf::from("/nonexistent/.credentials.json"),
                |_| async {
                    panic!("refresh should not be called for valid credentials");
                    #[allow(unreachable_code)]
                    Ok(parse_test_credentials(""))
                },
            )
            .await
            .unwrap();

            let stored = keychain::get_password(&service, &account).unwrap();
            assert_eq!(stored, json);
            cleanup_keychain(&service, &account);
        }
    }
}
