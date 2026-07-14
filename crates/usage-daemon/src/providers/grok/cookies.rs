//! Grok cookie-source policy built on the provider-neutral browser importer.

use crate::{
    config::ProviderConfig,
    keychain,
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
    // This path serves only the default Grok profile: environment wins over
    // the provider-level config and cookie file. Managed profiles never share
    // the default profile's browser cookies and authenticate from GROK_HOME.
    std::env::var("USAGE_TRACKER_GROK_COOKIE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            super::settings::provider(config)
                .ok()
                .and_then(|settings| settings.cookie_header)
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            dirs::home_dir().and_then(|home| {
                std::fs::read_to_string(home.join(".usagetracker/grok.cookie")).ok()
            })
        })
        .filter(|value| !value.trim().is_empty())
}

pub(super) async fn cached_candidate() -> Option<CookieCandidate> {
    let raw = tokio::task::spawn_blocking(|| {
        keychain::get_password(CACHE_SERVICE, super::PROVIDER_ID).ok()
    })
    .await
    .ok()??;
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

pub(super) async fn store(candidate: &CookieCandidate) {
    let header = candidate.header.clone();
    let _ = tokio::task::spawn_blocking(move || {
        keychain::set_password_if_changed(CACHE_SERVICE, super::PROVIDER_ID, &header)
    })
    .await;
}

pub(super) async fn clear_cache() {
    let _ = tokio::task::spawn_blocking(|| {
        keychain::delete_password(CACHE_SERVICE, super::PROVIDER_ID)
    })
    .await;
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
