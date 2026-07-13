//! Provider-neutral macOS browser-cookie discovery and decryption.
//!
//! Provider modules supply domains, required cookie names, and an optional
//! allowlist. Browser profiles remain separate candidates so an expired login
//! never masks a healthy session in another profile.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use aes::Aes128;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection;
use sha1::Sha1;
use uuid::Uuid;

use crate::keychain;

use super::{ProviderError, ProviderErrorKind};

type ChromiumKey = [u8; 16];
type ChromiumKeyCache = BTreeMap<(&'static str, &'static str), ChromiumKey>;

static CHROMIUM_KEY_CACHE: OnceLock<Mutex<ChromiumKeyCache>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct ImportedCookieSession {
    pub(crate) header: String,
    pub(crate) source_label: String,
}

#[derive(Clone, Copy)]
pub(crate) struct BrowserCookieStore {
    pub(crate) label: &'static str,
    pub(crate) app_support_path: &'static str,
    pub(crate) keychain_service: &'static str,
    pub(crate) keychain_account: &'static str,
    pub(crate) kind: BrowserCookieStoreKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum BrowserCookieStoreKind {
    Chromium,
    Firefox,
}

pub(crate) fn import_browser_cookie_sessions(
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Vec<ImportedCookieSession>, ProviderError> {
    import_browser_cookie_sessions_from(
        &browser_import_order(),
        domains,
        required_cookie_names,
        allowed_cookie_names,
    )
}

pub(crate) fn import_chrome_cookie_sessions(
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Vec<ImportedCookieSession>, ProviderError> {
    import_browser_cookie_sessions_from(
        &[chrome_cookie_store()],
        domains,
        required_cookie_names,
        allowed_cookie_names,
    )
}

fn import_browser_cookie_sessions_from(
    browsers: &[BrowserCookieStore],
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Vec<ImportedCookieSession>, ProviderError> {
    let home = dirs::home_dir().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "failed to resolve home directory for browser cookie import",
        )
    })?;
    let mut sessions = Vec::new();
    let mut failures = Vec::new();
    for &browser in browsers {
        for (index, cookie_path) in browser_cookie_paths(&home, browser).into_iter().enumerate() {
            match import_cookie_db(
                &cookie_path,
                browser,
                domains,
                required_cookie_names,
                allowed_cookie_names,
            ) {
                Ok(Some(header)) => sessions.push(ImportedCookieSession {
                    header,
                    source_label: format!("{} profile {}", browser.label, index + 1),
                }),
                Ok(None) => {}
                Err(err) => failures.push(format!(
                    "{} at {}: {}",
                    browser.label,
                    cookie_path.display(),
                    err.short_message()
                )),
            }
        }
    }
    if !sessions.is_empty() {
        return Ok(sessions);
    }
    if failures.is_empty() {
        Err(missing_cookie_error())
    } else {
        Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            format!(
                "no usable browser sessions were found; import errors: {}",
                failures.join("; ")
            ),
        ))
    }
}

pub(crate) fn missing_cookie_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::CredentialsMissing,
        "no matching auth cookies were found in supported browser stores",
    )
}

fn import_cookie_db(
    cookie_path: &Path,
    browser: BrowserCookieStore,
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Option<String>, ProviderError> {
    let copy_path = std::env::temp_dir().join(format!(
        "usagetracker-{}-cookies-{}.sqlite",
        browser.label.replace(' ', "-").to_ascii_lowercase(),
        Uuid::new_v4()
    ));
    std::fs::copy(cookie_path, &copy_path).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("failed to copy browser cookie database: {err}"),
        )
    })?;
    let result = import_cookie_db_copy(
        &copy_path,
        browser,
        domains,
        required_cookie_names,
        allowed_cookie_names,
    );
    let _ = std::fs::remove_file(copy_path);
    result
}

pub(crate) fn import_cookie_db_copy(
    cookie_path: &Path,
    browser: BrowserCookieStore,
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Option<String>, ProviderError> {
    let conn = Connection::open(cookie_path).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("failed to open browser cookie database copy: {err}"),
        )
    })?;
    import_cookie_connection_with_store(
        &conn,
        browser,
        domains,
        required_cookie_names,
        allowed_cookie_names,
    )
}

#[cfg(test)]
pub(crate) fn import_cookie_connection(
    conn: &Connection,
    kind: BrowserCookieStoreKind,
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Option<String>, ProviderError> {
    let browser = browser_import_order()
        .into_iter()
        .find(|browser| browser.kind == kind)
        .unwrap_or(BrowserCookieStore {
            label: "Browser",
            app_support_path: "",
            keychain_service: "",
            keychain_account: "",
            kind,
        });
    import_cookie_connection_with_store(
        conn,
        browser,
        domains,
        required_cookie_names,
        allowed_cookie_names,
    )
}

fn import_cookie_connection_with_store(
    conn: &Connection,
    browser: BrowserCookieStore,
    domains: &[&str],
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Result<Option<String>, ProviderError> {
    if domains.is_empty() {
        return Ok(None);
    }
    let (host_column, table, encrypted_column, expiry_column, order) = match browser.kind {
        BrowserCookieStoreKind::Chromium => (
            "host_key",
            "cookies",
            "encrypted_value",
            "expires_utc",
            "expires_utc DESC, last_access_utc DESC, creation_utc DESC",
        ),
        BrowserCookieStoreKind::Firefox => (
            "host",
            "moz_cookies",
            "X''",
            "expiry",
            "expiry DESC, lastAccessed DESC, creationTime DESC",
        ),
    };
    let domain_filter = std::iter::repeat_n(format!("{host_column} LIKE ?"), domains.len())
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!(
        "SELECT {host_column}, name, value, {encrypted_column}, {expiry_column} FROM {table} \
         WHERE ({domain_filter}) ORDER BY {order}"
    );
    let patterns = domains
        .iter()
        .map(|domain| format!("%{}", domain.trim_start_matches('.')))
        .collect::<Vec<_>>();
    let mut stmt = conn.prepare(&sql).map_err(cookie_db_error)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(patterns.iter()), |row| {
            Ok(BrowserCookieRow {
                host: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
                encrypted_value: row.get(3)?,
                expires_at: row.get(4)?,
            })
        })
        .map_err(cookie_db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(cookie_db_error)?;

    let mut cookies = BTreeMap::new();
    for row in rows {
        if !host_matches(&row.host, domains)
            || allowed_cookie_names.is_some_and(|allowed| !allowed.contains(&row.name.as_str()))
            || cookies.contains_key(&row.name)
            || !plausible_cookie_name(&row.name)
            || cookie_is_expired(row.expires_at, browser.kind, chrono::Utc::now().timestamp())
        {
            continue;
        }
        if let Some(value) = browser_cookie_value(&row, browser) {
            if plausible_cookie_value(&value) {
                cookies.insert(row.name, value);
            }
        }
    }
    if !required_cookie_names
        .iter()
        .any(|name| cookies.contains_key(*name))
    {
        return Ok(None);
    }
    Ok(Some(format_cookie_header(&cookies, allowed_cookie_names)))
}

pub(crate) fn normalize_cookie_header(
    raw: &str,
    required_cookie_names: &[&str],
    allowed_cookie_names: Option<&[&str]>,
) -> Option<String> {
    let mut cookies = BTreeMap::new();
    for part in raw.trim().split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if allowed_cookie_names.is_some_and(|allowed| !allowed.contains(&name))
            || !plausible_cookie_name(name)
            || !plausible_cookie_value(value)
        {
            continue;
        }
        cookies.insert(name.to_string(), value.to_string());
    }
    if !required_cookie_names
        .iter()
        .any(|name| cookies.contains_key(*name))
    {
        return None;
    }
    Some(format_cookie_header(&cookies, allowed_cookie_names))
}

fn host_matches(host: &str, domains: &[&str]) -> bool {
    let host = host.trim().trim_start_matches('.').to_ascii_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.trim().trim_start_matches('.').to_ascii_lowercase();
        host == domain || host.ends_with(&format!(".{domain}"))
    })
}

fn format_cookie_header(
    cookies: &BTreeMap<String, String>,
    ordered_names: Option<&[&str]>,
) -> String {
    if let Some(ordered_names) = ordered_names {
        return ordered_names
            .iter()
            .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
            .collect::<Vec<_>>()
            .join("; ");
    }
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn browser_import_order() -> Vec<BrowserCookieStore> {
    vec![
        chrome_cookie_store(),
        BrowserCookieStore {
            label: "Dia",
            app_support_path: "Dia",
            keychain_service: "Dia Safe Storage",
            keychain_account: "Dia",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Firefox",
            app_support_path: "Firefox",
            keychain_service: "",
            keychain_account: "",
            kind: BrowserCookieStoreKind::Firefox,
        },
        BrowserCookieStore {
            label: "Brave",
            app_support_path: "BraveSoftware/Brave-Browser",
            keychain_service: "Brave Safe Storage",
            keychain_account: "Brave",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Edge",
            app_support_path: "Microsoft Edge",
            keychain_service: "Microsoft Edge Safe Storage",
            keychain_account: "Microsoft Edge",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Arc",
            app_support_path: "Arc/User Data",
            keychain_service: "Arc Safe Storage",
            keychain_account: "Arc",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Chromium",
            app_support_path: "Chromium",
            keychain_service: "Chromium Safe Storage",
            keychain_account: "Chromium",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Vivaldi",
            app_support_path: "Vivaldi",
            keychain_service: "Vivaldi Safe Storage",
            keychain_account: "Vivaldi",
            kind: BrowserCookieStoreKind::Chromium,
        },
    ]
}

fn chrome_cookie_store() -> BrowserCookieStore {
    BrowserCookieStore {
        label: "Chrome",
        app_support_path: "Google/Chrome",
        keychain_service: "Chrome Safe Storage",
        keychain_account: "Chrome",
        kind: BrowserCookieStoreKind::Chromium,
    }
}

fn browser_cookie_paths(home: &Path, browser: BrowserCookieStore) -> Vec<PathBuf> {
    let root = home
        .join("Library/Application Support")
        .join(browser.app_support_path);
    let mut paths = Vec::new();
    if browser.kind == BrowserCookieStoreKind::Firefox {
        if let Ok(entries) = std::fs::read_dir(root.join("Profiles")) {
            paths.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("cookies.sqlite")),
            );
        }
        paths.push(root.join("cookies.sqlite"));
    } else if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            paths.push(entry.path().join("Network/Cookies"));
            paths.push(entry.path().join("Cookies"));
        }
    }
    paths.sort_by_key(|path| {
        let network_priority = usize::from(!path.to_string_lossy().contains("/Network/"));
        (network_priority, path.clone())
    });
    paths.dedup();
    paths.into_iter().filter(|path| path.exists()).collect()
}

struct BrowserCookieRow {
    host: String,
    name: String,
    value: String,
    encrypted_value: Vec<u8>,
    expires_at: i64,
}

fn cookie_is_expired(expires_at: i64, kind: BrowserCookieStoreKind, now_unix: i64) -> bool {
    if expires_at <= 0 {
        return false;
    }
    let unix_seconds = match kind {
        BrowserCookieStoreKind::Chromium => expires_at
            .checked_div(1_000_000)
            .and_then(|seconds| seconds.checked_sub(11_644_473_600)),
        BrowserCookieStoreKind::Firefox => Some(expires_at),
    };
    unix_seconds.is_some_and(|expires| expires <= now_unix)
}

fn browser_cookie_value(row: &BrowserCookieRow, browser: BrowserCookieStore) -> Option<String> {
    if !row.value.trim().is_empty() {
        return Some(row.value.trim().to_string());
    }
    if browser.kind == BrowserCookieStoreKind::Firefox {
        return None;
    }
    decrypt_chromium_cookie(&row.encrypted_value, &row.host, browser)
}

fn decrypt_chromium_cookie(
    encrypted_value: &[u8],
    _host: &str,
    browser: BrowserCookieStore,
) -> Option<String> {
    if encrypted_value.is_empty() {
        return None;
    }
    if !encrypted_value.starts_with(b"v10") && !encrypted_value.starts_with(b"v11") {
        return String::from_utf8(encrypted_value.to_vec()).ok();
    }
    let key = chromium_decryption_key(browser)?;
    let mut buffer = encrypted_value[3..].to_vec();
    let plaintext = cbc::Decryptor::<Aes128>::new(&key.into(), &[b' '; 16].into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;
    let value = [plaintext, plaintext.get(32..).unwrap_or_default()]
        .into_iter()
        .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .find(|value| plausible_cookie_value(value));
    value
}

fn chromium_decryption_key(browser: BrowserCookieStore) -> Option<ChromiumKey> {
    let cache = CHROMIUM_KEY_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cache_key = (browser.keychain_service, browser.keychain_account);
    if let Some(key) = cache.get(&cache_key) {
        return Some(*key);
    }

    // Keep this lock while reading Keychain so concurrent provider imports cannot
    // trigger duplicate authorization prompts for the same browser credential.
    let password =
        keychain::get_password(browser.keychain_service, browser.keychain_account).ok()?;
    let mut key = [0_u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    cache.insert(cache_key, key);
    Some(key)
}

fn plausible_cookie_name(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, ';' | '='))
}

fn plausible_cookie_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == ';')
}

fn cookie_db_error(err: rusqlite::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderUnavailable,
        format!("browser cookie database query failed: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_browser_specific_expiry_epochs_and_keeps_session_cookies() {
        let now = 1_800_000_000;
        let chromium_epoch_offset = 11_644_473_600_i64;
        let chrome_expired = (chromium_epoch_offset + now - 1) * 1_000_000;
        let chrome_valid = (chromium_epoch_offset + now + 1) * 1_000_000;
        assert!(cookie_is_expired(
            chrome_expired,
            BrowserCookieStoreKind::Chromium,
            now
        ));
        assert!(!cookie_is_expired(
            chrome_valid,
            BrowserCookieStoreKind::Chromium,
            now
        ));
        assert!(cookie_is_expired(
            now - 1,
            BrowserCookieStoreKind::Firefox,
            now
        ));
        assert!(!cookie_is_expired(0, BrowserCookieStoreKind::Firefox, now));
    }
}
