use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{paths::expand_home_path, DiscoveredAccount},
};

pub(crate) const DEFAULT_PROFILE_ID: &str = "default";

#[derive(Clone, Debug)]
pub(super) struct GrokProfile {
    pub(super) id: String,
    pub(super) display_name: Option<String>,
    pub(super) grok_home: PathBuf,
    pub(super) allows_legacy_browser_auth: bool,
}

pub(crate) fn default_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(expand_home_path)
        .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
        .ok_or_else(|| anyhow::anyhow!("failed to resolve GROK_HOME"))
}

pub(crate) fn normalized_id(configured: Option<&str>, index: usize) -> String {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if index == 0 {
                DEFAULT_PROFILE_ID.to_string()
            } else {
                format!("profile-{}", index + 1)
            }
        })
}

pub(super) fn resolve(config: &ProviderConfig) -> anyhow::Result<Vec<GrokProfile>> {
    let default_home = default_home()?;
    let configured = if config.profiles.is_empty() {
        let mut profile = ProviderProfileConfig {
            id: Some(DEFAULT_PROFILE_ID.to_string()),
            ..ProviderProfileConfig::default()
        };
        super::settings::update_profile(&mut profile, |settings| {
            settings.grok_home = Some(default_home.clone());
        })?;
        vec![profile]
    } else {
        config.profiles.clone()
    };

    let profiles = configured
        .into_iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, profile)| {
            let settings = super::settings::profile(&profile)?;
            let id = normalized_id(profile.id.as_deref(), index);
            let grok_home = match settings.grok_home {
                Some(path) => expand_home_path(path),
                None if id == DEFAULT_PROFILE_ID => default_home.clone(),
                None => anyhow::bail!("Grok profile {id} is missing its home directory"),
            };
            let allows_legacy_browser_auth = id == DEFAULT_PROFILE_ID && grok_home == default_home;
            Ok(GrokProfile {
                id,
                display_name: profile.display_name,
                grok_home,
                allows_legacy_browser_auth,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut ids = BTreeSet::new();
    for profile in &profiles {
        if !ids.insert(profile.id.as_str()) {
            anyhow::bail!("duplicate Grok profile id: {}", profile.id);
        }
    }
    Ok(profiles)
}

pub(super) fn deduplicate_accounts(accounts: &mut Vec<DiscoveredAccount>) {
    let mut canonical_profiles = BTreeMap::<String, String>::new();
    accounts.retain(|account| {
        let profile_id = account.profile_id.as_deref().unwrap_or("unknown");
        if let Some(canonical_profile_id) = canonical_profiles.get(&account.external_account_id) {
            tracing::warn!(
                external_account_id = account.external_account_id.as_str(),
                canonical_profile_id = canonical_profile_id.as_str(),
                duplicate_profile_id = profile_id,
                "duplicate Grok account ignored; each account can only be connected once"
            );
            false
        } else {
            canonical_profiles.insert(account.external_account_id.clone(), profile_id.to_string());
            true
        }
    });
}
