//! OpenCode-specific manual/cache policy layered on shared browser import.

use keyring::Entry;

use crate::providers::{
    browser_cookies::{self, ImportedCookieSession},
    ProviderError,
};

use super::{COOKIE_CACHE_SERVICE, COOKIE_NAMES, OPENCODE_GO_PROVIDER_ID};

#[cfg(test)]
pub(super) use crate::providers::browser_cookies::{BrowserCookieStore, BrowserCookieStoreKind};

pub(super) fn read_cookie_file(provider_id: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    std::fs::read_to_string(
        home.join(".usagetracker")
            .join(format!("{provider_id}.cookie")),
    )
    .ok()
}

pub(super) fn load_cached_cookie_header(provider_id: &str) -> Option<String> {
    Entry::new(COOKIE_CACHE_SERVICE, provider_id)
        .ok()?
        .get_password()
        .ok()
        .and_then(|value| normalize_cookie_header(&value))
}

pub(super) fn store_cached_cookie_header(provider_id: &str, cookie_header: &str) {
    if let Ok(entry) = Entry::new(COOKIE_CACHE_SERVICE, provider_id) {
        let _ = entry.set_password(cookie_header);
    }
}

pub(super) fn clear_cached_cookie_header(provider_id: &str) {
    if let Ok(entry) = Entry::new(COOKIE_CACHE_SERVICE, provider_id) {
        let _ = entry.delete_credential();
    }
}

pub(crate) async fn clear_cached_cookie_cache() -> anyhow::Result<()> {
    tokio::task::spawn_blocking(|| clear_cached_cookie_header(OPENCODE_GO_PROVIDER_ID)).await?;
    Ok(())
}

pub(super) fn import_browser_cookie_header(_provider_id: &str) -> Result<String, ProviderError> {
    browser_cookies::import_browser_cookie_sessions(
        &["opencode.ai", "app.opencode.ai"],
        &COOKIE_NAMES,
        Some(&COOKIE_NAMES),
    )
    .and_then(first_header)
}

fn first_header(sessions: Vec<ImportedCookieSession>) -> Result<String, ProviderError> {
    sessions
        .into_iter()
        .next()
        .map(|session| session.header)
        .ok_or_else(browser_cookies::missing_cookie_error)
}

#[cfg(test)]
pub(super) fn import_cookie_db_copy(
    cookie_path: &std::path::Path,
    browser: BrowserCookieStore,
) -> Result<Option<String>, ProviderError> {
    browser_cookies::import_cookie_db_copy(
        cookie_path,
        browser,
        &["opencode.ai", "app.opencode.ai"],
        &COOKIE_NAMES,
        Some(&COOKIE_NAMES),
    )
}

#[cfg(test)]
pub(super) fn import_firefox_cookie_db_copy(
    conn: &rusqlite::Connection,
) -> Result<Option<String>, ProviderError> {
    browser_cookies::import_cookie_connection(
        conn,
        BrowserCookieStoreKind::Firefox,
        &["opencode.ai", "app.opencode.ai"],
        &COOKIE_NAMES,
        Some(&COOKIE_NAMES),
    )
}

pub(super) fn normalize_cookie_header(raw: &str) -> Option<String> {
    browser_cookies::normalize_cookie_header(raw, &COOKIE_NAMES, Some(&COOKIE_NAMES))
}

pub(super) fn is_auth_error(error: &ProviderError) -> bool {
    matches!(
        error.kind(),
        crate::providers::ProviderErrorKind::Unauthorized
            | crate::providers::ProviderErrorKind::CredentialsInvalid
    )
}
