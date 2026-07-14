use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::profile_service::unique_profile_id,
    providers::{
        grok::{
            default_grok_home, normalized_profile_id, settings, DEFAULT_PROFILE_ID, PROVIDER_ID,
        },
        paths::expand_home_path,
    },
    runtime::managed_profiles,
};

pub(crate) struct GrokLoginTarget {
    pub(crate) profile_id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) grok_home: PathBuf,
}

pub(crate) fn select_login_target(
    provider: &mut ProviderConfig,
    connected_profiles: &BTreeSet<String>,
    display_name: Option<String>,
) -> anyhow::Result<GrokLoginTarget> {
    materialize_connected_legacy_profile(provider, connected_profiles)?;
    if let Some(target) = pending_profile(provider, connected_profiles)? {
        return Ok(target);
    }
    if connected_profiles.is_empty()
        && (provider.profiles.is_empty() || has_default_profile(provider))
    {
        return ensure_login_profile(provider, Some(DEFAULT_PROFILE_ID));
    }
    create_managed_profile(provider, display_name)
}

fn materialize_connected_legacy_profile(
    provider: &mut ProviderConfig,
    connected_profiles: &BTreeSet<String>,
) -> anyhow::Result<()> {
    if provider.profiles.is_empty() && connected_profiles.contains(DEFAULT_PROFILE_ID) {
        provider.profiles.push(default_profile()?);
    }
    Ok(())
}

pub(crate) fn select_browser_login_target(
    provider: &mut ProviderConfig,
) -> anyhow::Result<GrokLoginTarget> {
    if provider.profiles.is_empty() || has_default_profile(provider) {
        let target = ensure_login_profile(provider, Some(DEFAULT_PROFILE_ID))?;
        if target.grok_home == default_grok_home()? {
            return Ok(target);
        }
    }
    anyhow::bail!("connecting this Grok account requires the Grok Build CLI")
}

fn has_default_profile(provider: &ProviderConfig) -> bool {
    provider
        .profiles
        .iter()
        .enumerate()
        .any(|(index, profile)| {
            normalized_profile_id(profile.id.as_deref(), index) == DEFAULT_PROFILE_ID
        })
}

pub(crate) fn ensure_login_profile(
    provider: &mut ProviderConfig,
    requested_profile_id: Option<&str>,
) -> anyhow::Result<GrokLoginTarget> {
    if provider.profiles.is_empty() {
        provider.profiles.push(default_profile()?);
    }
    if let Some(requested_profile_id) = requested_profile_id {
        if requested_profile_id == DEFAULT_PROFILE_ID {
            if let Some((index, profile)) =
                provider
                    .profiles
                    .iter_mut()
                    .enumerate()
                    .find(|(index, profile)| {
                        normalized_profile_id(profile.id.as_deref(), *index) == DEFAULT_PROFILE_ID
                    })
            {
                profile.enabled = true;
                profile.deleted = false;
                if settings::profile(profile)?.grok_home.is_none() {
                    let default_home = default_grok_home()?;
                    settings::update_profile(profile, |settings| {
                        settings.grok_home = Some(default_home);
                    })?;
                }
                return login_target(profile, index);
            }
        }
        let (index, profile) = provider
            .profiles
            .iter_mut()
            .enumerate()
            .find(|(index, profile)| {
                !profile.deleted
                    && normalized_profile_id(profile.id.as_deref(), *index) == requested_profile_id
            })
            .ok_or_else(|| {
                anyhow::anyhow!("Grok profile {requested_profile_id} is no longer configured")
            })?;
        profile.enabled = true;
        return login_target(profile, index);
    }
    if let Some((index, profile)) = provider
        .profiles
        .iter()
        .enumerate()
        .find(|(_, profile)| profile.enabled && !profile.deleted)
    {
        return login_target(profile, index);
    }
    create_managed_profile(provider, None)
}

fn pending_profile(
    provider: &ProviderConfig,
    connected_profiles: &BTreeSet<String>,
) -> anyhow::Result<Option<GrokLoginTarget>> {
    for (index, profile) in provider.profiles.iter().enumerate().rev() {
        if !profile.enabled || profile.deleted {
            continue;
        }
        let profile_id = normalized_profile_id(profile.id.as_deref(), index);
        if !connected_profiles.contains(&profile_id) {
            return login_target(profile, index).map(Some);
        }
    }
    Ok(None)
}

fn create_managed_profile(
    provider: &mut ProviderConfig,
    display_name: Option<String>,
) -> anyhow::Result<GrokLoginTarget> {
    let profile_id = unique_profile_id(&provider.profiles, display_name.as_deref());
    let grok_home = managed_profiles::profile_home(PROVIDER_ID, &profile_id)?;
    std::fs::create_dir_all(&grok_home)?;
    let mut profile = ProviderProfileConfig {
        id: Some(profile_id.clone()),
        display_name: display_name.clone(),
        ..ProviderProfileConfig::default()
    };
    settings::update_profile(&mut profile, |settings| {
        settings.grok_home = Some(grok_home.clone());
    })?;
    provider.profiles.push(profile);
    Ok(GrokLoginTarget {
        profile_id,
        display_name,
        grok_home,
    })
}

fn default_profile() -> anyhow::Result<ProviderProfileConfig> {
    let default_home = default_grok_home()?;
    let mut profile = ProviderProfileConfig {
        id: Some(DEFAULT_PROFILE_ID.to_string()),
        ..ProviderProfileConfig::default()
    };
    settings::update_profile(&mut profile, |settings| {
        settings.grok_home = Some(default_home);
    })?;
    Ok(profile)
}

fn login_target(profile: &ProviderProfileConfig, index: usize) -> anyhow::Result<GrokLoginTarget> {
    let profile_id = normalized_profile_id(profile.id.as_deref(), index);
    let grok_home = match settings::profile(profile)?.grok_home {
        Some(path) => expand_home_path(path),
        None if profile_id == DEFAULT_PROFILE_ID => default_grok_home()?,
        None => anyhow::bail!("Grok profile {profile_id} is missing its home directory"),
    };
    Ok(GrokLoginTarget {
        profile_id,
        display_name: profile.display_name.clone(),
        grok_home,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, home: impl Into<PathBuf>) -> ProviderProfileConfig {
        let mut profile = ProviderProfileConfig {
            id: Some(id.to_string()),
            ..ProviderProfileConfig::default()
        };
        settings::update_profile(&mut profile, |settings| {
            settings.grok_home = Some(home.into());
        })
        .unwrap();
        profile
    }

    #[test]
    fn initial_connect_uses_an_explicit_non_default_profile() {
        let mut provider = ProviderConfig {
            profiles: vec![profile("work", "/tmp/grok-work")],
            ..ProviderConfig::default()
        };

        let target = select_login_target(&mut provider, &BTreeSet::new(), None).unwrap();

        assert_eq!(target.profile_id, "work");
        assert_eq!(target.grok_home, PathBuf::from("/tmp/grok-work"));
        assert_eq!(provider.profiles.len(), 1);
    }

    #[test]
    fn adding_to_a_legacy_account_materializes_the_implicit_default_profile() {
        let mut provider = ProviderConfig::default();
        let connected = BTreeSet::from([DEFAULT_PROFILE_ID.to_string()]);

        materialize_connected_legacy_profile(&mut provider, &connected).unwrap();

        assert_eq!(provider.profiles.len(), 1);
        assert_eq!(provider.profiles[0].id.as_deref(), Some(DEFAULT_PROFILE_ID));
    }

    #[test]
    fn browser_login_does_not_create_an_unusable_managed_profile() {
        let mut provider = ProviderConfig {
            profiles: vec![profile("work", "/tmp/grok-work")],
            ..ProviderConfig::default()
        };

        let error = select_browser_login_target(&mut provider).err().unwrap();

        assert!(error.to_string().contains("requires the Grok Build CLI"));
        assert_eq!(provider.profiles.len(), 1);
    }

    #[test]
    fn browser_login_rejects_a_default_id_with_an_isolated_home() {
        let isolated_home = default_grok_home().unwrap().join("isolated-test");
        let mut provider = ProviderConfig {
            profiles: vec![profile("default", isolated_home)],
            ..ProviderConfig::default()
        };

        let error = select_browser_login_target(&mut provider).err().unwrap();

        assert!(error.to_string().contains("requires the Grok Build CLI"));
    }

    #[test]
    fn unconnected_profile_with_credentials_is_reused() {
        let root =
            std::env::temp_dir().join(format!("grok-duplicate-profile-{}", uuid::Uuid::new_v4()));
        let duplicate_home = root.join("duplicate");
        std::fs::create_dir_all(&duplicate_home).unwrap();
        std::fs::write(duplicate_home.join("auth.json"), b"{}").unwrap();
        let mut provider = ProviderConfig {
            profiles: vec![
                profile("default", "/tmp/grok-default"),
                profile("duplicate", duplicate_home),
            ],
            ..ProviderConfig::default()
        };
        let connected = BTreeSet::from(["default".to_string()]);

        let target = select_login_target(&mut provider, &connected, None).unwrap();

        assert_eq!(target.profile_id, "duplicate");
        assert_eq!(provider.profiles.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn login_target_uses_the_canonical_profile_id() {
        let mut provider = ProviderConfig {
            profiles: vec![profile(" work ", "/tmp/grok-work")],
            ..ProviderConfig::default()
        };

        let target = ensure_login_profile(&mut provider, Some("work")).unwrap();

        assert_eq!(target.profile_id, "work");
    }
}
