//! Grok cookie-source policy built on the provider-neutral browser importer.

use keyring::Entry;

use crate::{
    config::ProviderConfig,
    providers::{browser_cookies, ProviderError},
};

const CACHE_SERVICE: &str = "usagetracker.grok.cookies";
const REQUIRED_NAMES: [&str; 2] = ["sso", "sso-rw"];

#[derive(Clone, Debug)]
pub(super) struct CookieCandidate {
    pub(super) header: String,
    pub(super) source: String,
    pub(super) browser_imported: bool,
}

pub(super) fn manual_candidate(config: &ProviderConfig) -> Option<CookieCandidate> {
    let raw = manual_raw(config)?;
    normalize(&raw).map(|header| CookieCandidate {
        header,
        source: "manual".to_string(),
        browser_imported: false,
    })
}

pub(super) fn manual_is_configured(config: &ProviderConfig) -> bool {
    manual_raw(config).is_some()
}

fn manual_raw(config: &ProviderConfig) -> Option<String> {
    std::env::var("USAGE_TRACKER_GROK_COOKIE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            config
                .cookie_header
                .clone()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            dirs::home_dir().and_then(|home| {
                std::fs::read_to_string(home.join(".usagetracker/grok.cookie")).ok()
            })
        })
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn cached_candidate() -> Option<CookieCandidate> {
    let raw = Entry::new(CACHE_SERVICE, super::PROVIDER_ID)
        .ok()?
        .get_password()
        .ok()?;
    normalize(&raw).map(|header| CookieCandidate {
        header,
        source: "keychain_cache".to_string(),
        browser_imported: false,
    })
}

pub(super) fn import_browser_candidates() -> Result<Vec<CookieCandidate>, ProviderError> {
    if cfg!(test)
        && std::env::var("USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT").as_deref() != Ok("1")
    {
        return Err(ProviderError::new(
            crate::providers::ProviderErrorKind::CredentialsMissing,
            "Grok browser cookie import is disabled in test processes",
        ));
    }
    browser_cookies::import_chrome_cookie_sessions(
        &["grok.com"],
        &REQUIRED_NAMES,
        Some(&REQUIRED_NAMES),
    )
    .map(|sessions| {
        sessions
            .into_iter()
            .map(|session| CookieCandidate {
                header: session.header,
                source: session.source_label,
                browser_imported: true,
            })
            .collect()
    })
}

pub(super) fn store(candidate: &CookieCandidate) {
    if let Ok(entry) = Entry::new(CACHE_SERVICE, super::PROVIDER_ID) {
        let _ = entry.set_password(&candidate.header);
    }
}

pub(super) fn clear_cache() {
    if let Ok(entry) = Entry::new(CACHE_SERVICE, super::PROVIDER_ID) {
        let _ = entry.delete_credential();
    }
}

fn normalize(raw: &str) -> Option<String> {
    browser_cookies::normalize_cookie_header(raw, &REQUIRED_NAMES, Some(&REQUIRED_NAMES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_complete_grok_session_and_rejects_unrelated_cookies() {
        assert_eq!(
            normalize("theme=dark; sso=session; other=value").as_deref(),
            Some("sso=session")
        );
        assert!(normalize("theme=dark; other=value").is_none());
    }
}
