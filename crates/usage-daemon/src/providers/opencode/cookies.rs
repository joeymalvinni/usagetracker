//! Manual, cached, and browser-imported OpenCode cookie resolution.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use aes::Aes128;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use keyring::Entry;
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection;
use sha1::Sha1;
use uuid::Uuid;

use crate::providers::{ProviderError, ProviderErrorKind};

use super::{COOKIE_CACHE_SERVICE, COOKIE_NAMES, OPENCODE_GO_PROVIDER_ID};

pub(super) fn read_cookie_file(provider_id: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let specific = home
        .join(".usagetracker")
        .join(format!("{}.cookie", provider_id));
    std::fs::read_to_string(&specific).ok()
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
    let home = dirs::home_dir().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "failed to resolve home directory for browser cookie import",
        )
    })?;
    let mut failures = Vec::new();
    for browser in browser_import_order() {
        let Ok(cookie_paths) = browser_cookie_paths(&home, browser) else {
            continue;
        };
        for cookie_path in cookie_paths {
            match import_cookie_db(&cookie_path, browser) {
                Ok(Some(header)) => return Ok(header),
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

    let detail = if failures.is_empty() {
        "no OpenCode auth cookies were found in supported browser stores".to_string()
    } else {
        format!(
            "no usable OpenCode auth cookies were found; import errors: {}",
            failures.join("; ")
        )
    };
    Err(ProviderError::new(
        ProviderErrorKind::CredentialsMissing,
        detail,
    ))
}

#[derive(Clone, Copy)]
pub(super) struct BrowserCookieStore {
    pub(super) label: &'static str,
    pub(super) app_support_path: &'static str,
    pub(super) keychain_service: &'static str,
    pub(super) keychain_account: &'static str,
    pub(super) kind: BrowserCookieStoreKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum BrowserCookieStoreKind {
    Chromium,
    Firefox,
}

fn browser_import_order() -> Vec<BrowserCookieStore> {
    let chrome = BrowserCookieStore {
        label: "Chrome",
        app_support_path: "Google/Chrome",
        keychain_service: "Chrome Safe Storage",
        keychain_account: "Chrome",
        kind: BrowserCookieStoreKind::Chromium,
    };
    let dia = BrowserCookieStore {
        label: "Dia",
        app_support_path: "Dia",
        keychain_service: "Dia Safe Storage",
        keychain_account: "Dia",
        kind: BrowserCookieStoreKind::Chromium,
    };
    let firefox = BrowserCookieStore {
        label: "Firefox",
        app_support_path: "Firefox",
        keychain_service: "",
        keychain_account: "",
        kind: BrowserCookieStoreKind::Firefox,
    };
    let common = vec![
        chrome,
        dia,
        firefox,
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
    ];
    common
}

fn browser_cookie_paths(
    home: &Path,
    browser: BrowserCookieStore,
) -> Result<Vec<PathBuf>, ProviderError> {
    let root = home
        .join("Library")
        .join("Application Support")
        .join(browser.app_support_path);
    if browser.kind == BrowserCookieStoreKind::Firefox {
        let mut paths = Vec::new();
        let profile_root = root.join("Profiles");
        if let Ok(entries) = std::fs::read_dir(&profile_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    paths.push(path.join("cookies.sqlite"));
                }
            }
        }
        paths.push(root.join("cookies.sqlite"));
        paths.sort();
        paths.dedup();
        return Ok(paths.into_iter().filter(|path| path.exists()).collect());
    }

    let mut paths = Vec::new();
    for profile in [
        "Default",
        "Profile 1",
        "Profile 2",
        "Profile 3",
        "Profile 4",
    ] {
        paths.push(root.join(profile).join("Network/Cookies"));
        paths.push(root.join(profile).join("Cookies"));
    }
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                paths.push(path.join("Network/Cookies"));
                paths.push(path.join("Cookies"));
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths.into_iter().filter(|path| path.exists()).collect())
}

fn import_cookie_db(
    cookie_path: &Path,
    browser: BrowserCookieStore,
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
    let result = import_cookie_db_copy(&copy_path, browser);
    let _ = std::fs::remove_file(copy_path);
    result
}

pub(super) fn import_cookie_db_copy(
    cookie_path: &Path,
    browser: BrowserCookieStore,
) -> Result<Option<String>, ProviderError> {
    let conn = Connection::open(cookie_path).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("failed to open browser cookie database copy: {err}"),
        )
    })?;
    if browser.kind == BrowserCookieStoreKind::Firefox {
        return import_firefox_cookie_db_copy(&conn);
    }

    let mut stmt = conn
        .prepare(
            r#"
            SELECT host_key, name, value, encrypted_value
            FROM cookies
            WHERE (host_key LIKE '%opencode.ai' OR host_key LIKE '%app.opencode.ai')
              AND name IN ('auth', '__Host-auth')
            ORDER BY expires_utc DESC, last_access_utc DESC, creation_utc DESC
            "#,
        )
        .map_err(browser_cookie_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(BrowserCookieRow {
                host_key: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
                encrypted_value: row.get(3)?,
            })
        })
        .map_err(browser_cookie_db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(browser_cookie_db_error)?;

    let mut cookies = BTreeMap::new();
    for row in rows {
        if !COOKIE_NAMES.contains(&row.name.as_str()) {
            continue;
        }
        if cookies.contains_key(&row.name) {
            continue;
        }
        if let Some(value) = browser_cookie_value(&row, browser) {
            cookies.insert(row.name, value);
        }
    }

    if cookies.is_empty() {
        return Ok(None);
    }
    let header = COOKIE_NAMES
        .iter()
        .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; ");
    normalize_cookie_header(&header).map(Some).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "browser cookie import produced an empty auth cookie header",
        )
    })
}

pub(super) fn import_firefox_cookie_db_copy(
    conn: &Connection,
) -> Result<Option<String>, ProviderError> {
    let mut stmt = conn
        .prepare(
            r#"
            SELECT host, name, value
            FROM moz_cookies
            WHERE (host LIKE '%opencode.ai' OR host LIKE '%app.opencode.ai')
              AND name IN ('auth', '__Host-auth')
            ORDER BY expiry DESC, lastAccessed DESC, creationTime DESC
            "#,
        )
        .map_err(browser_cookie_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(browser_cookie_db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(browser_cookie_db_error)?;

    let mut cookies = BTreeMap::new();
    for (_host, name, value) in rows {
        if !COOKIE_NAMES.contains(&name.as_str()) || cookies.contains_key(&name) {
            continue;
        }
        let value = value.trim();
        if !value.is_empty() {
            cookies.insert(name, value.to_string());
        }
    }

    if cookies.is_empty() {
        return Ok(None);
    }
    let header = COOKIE_NAMES
        .iter()
        .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(normalize_cookie_header(&header))
}

pub(super) struct BrowserCookieRow {
    host_key: String,
    name: String,
    value: String,
    encrypted_value: Vec<u8>,
}

fn browser_cookie_value(row: &BrowserCookieRow, browser: BrowserCookieStore) -> Option<String> {
    let value = row.value.trim();
    if !value.is_empty() {
        return Some(value.to_string());
    }
    decrypt_chromium_cookie(&row.encrypted_value, &row.host_key, browser)
}

fn decrypt_chromium_cookie(
    encrypted_value: &[u8],
    host_key: &str,
    browser: BrowserCookieStore,
) -> Option<String> {
    if encrypted_value.is_empty() {
        return None;
    }
    if !encrypted_value.starts_with(b"v10") && !encrypted_value.starts_with(b"v11") {
        return String::from_utf8(encrypted_value.to_vec()).ok();
    }

    let password = Entry::new(browser.keychain_service, browser.keychain_account)
        .ok()?
        .get_password()
        .ok()?;
    let mut key = [0_u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    let iv = [b' '; 16];
    let ciphertext = &encrypted_value[3..];
    let mut buffer = ciphertext.to_vec();
    let plaintext = cbc::Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;

    if let Ok(value) = String::from_utf8(plaintext.to_vec()) {
        if is_plausible_cookie_value(&value) {
            return Some(value);
        }
    }

    if plaintext.len() > 32 {
        if let Ok(value) = String::from_utf8(plaintext[32..].to_vec()) {
            if is_plausible_cookie_value(&value) {
                return Some(value);
            }
        }
    }

    if let Ok(value) = String::from_utf8(plaintext.to_vec()) {
        return Some(value);
    }

    if plaintext.len() > 32 {
        if let Ok(value) = String::from_utf8(plaintext[32..].to_vec()) {
            return Some(value);
        }
    }

    let _ = host_key;
    None
}

fn is_plausible_cookie_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
}

fn browser_cookie_db_error(err: rusqlite::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderUnavailable,
        format!("browser cookie database query failed: {err}"),
    )
}

pub(super) fn normalize_cookie_header(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let allowed = ["auth", "__Host-auth"];
    let filtered = raw
        .split(';')
        .filter_map(|part| {
            let part = part.trim();
            let (name, value) = part.split_once('=')?;
            allowed
                .iter()
                .any(|allowed_name| *allowed_name == name.trim())
                .then_some(value.trim())
                .filter(|value| is_plausible_cookie_value(value))
                .map(|value| format!("{}={value}", name.trim()))
        })
        .collect::<Vec<_>>();
    (!filtered.is_empty()).then(|| filtered.join("; "))
}

pub(super) fn is_auth_error(error: &ProviderError) -> bool {
    matches!(
        error.kind(),
        ProviderErrorKind::Unauthorized | ProviderErrorKind::CredentialsInvalid
    )
}
