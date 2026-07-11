use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{
        claude::{keychain_service_for_config_dir, PROVIDER_ID as CLAUDE_PROVIDER_ID},
        codex::PROVIDER_ID as CODEX_PROVIDER_ID,
        paths::expand_home_path,
    },
    runtime::managed_profiles,
};

pub(crate) struct ClaudeLoginTarget {
    pub(crate) profile_id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) config_dir: Option<PathBuf>,
}

pub(crate) fn codex_profile_home(profile_id: &str) -> anyhow::Result<PathBuf> {
    managed_profiles::profile_home(CODEX_PROVIDER_ID, profile_id)
}

fn claude_profile_home(profile_id: &str) -> anyhow::Result<PathBuf> {
    managed_profiles::profile_home(CLAUDE_PROVIDER_ID, profile_id)
}

pub(crate) fn pending_codex_profile(
    provider: &ProviderConfig,
) -> Option<(String, PathBuf, Option<String>)> {
    provider.profiles.iter().rev().find_map(|profile| {
        if !profile.enabled || profile.deleted {
            return None;
        }
        let profile_id = profile.id.clone()?;
        let profile_path = expand_home_path(profile.codex_home.clone()?);
        if !managed_profiles::is_managed_profile(&profile_path, CODEX_PROVIDER_ID) {
            return None;
        }
        let auth_path = profile
            .auth_path
            .clone()
            .map(expand_home_path)
            .unwrap_or_else(|| profile_path.join("auth.json"));
        (!auth_path.exists()).then(|| (profile_id, profile_path, profile.display_name.clone()))
    })
}

pub(crate) fn default_codex_profile() -> anyhow::Result<ProviderProfileConfig> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("failed to resolve home directory"))?;
    Ok(ProviderProfileConfig {
        id: Some("default".to_string()),
        display_name: None,
        codex_home: Some(home.join(".codex")),
        ..ProviderProfileConfig::default()
    })
}

pub(crate) fn ensure_claude_login_profile(
    provider: &mut ProviderConfig,
    requested_profile_id: Option<&str>,
) -> anyhow::Result<ClaudeLoginTarget> {
    if provider.profiles.is_empty() && requested_profile_id == Some("default") {
        return Ok(ClaudeLoginTarget {
            profile_id: "default".to_string(),
            display_name: None,
            config_dir: None,
        });
    }
    if let Some(requested_profile_id) = requested_profile_id {
        if let Some((index, profile)) =
            provider
                .profiles
                .iter_mut()
                .enumerate()
                .find(|(_, profile)| {
                    !profile.deleted && profile.id.as_deref() == Some(requested_profile_id)
                })
        {
            profile.enabled = true;
            return Ok(claude_login_target(profile, index));
        }
        anyhow::bail!("Claude profile {requested_profile_id} is no longer configured");
    }

    if let Some((index, profile)) = provider
        .profiles
        .iter()
        .enumerate()
        .find(|(_, profile)| profile.enabled && !profile.deleted)
    {
        return Ok(claude_login_target(profile, index));
    }

    create_managed_claude_profile(provider, None)
}

pub(crate) fn create_managed_claude_profile(
    provider: &mut ProviderConfig,
    display_name: Option<String>,
) -> anyhow::Result<ClaudeLoginTarget> {
    let profile_id = unique_profile_id(&provider.profiles, display_name.as_deref());
    let config_dir = claude_profile_home(&profile_id)?;
    push_managed_claude_profile(provider, profile_id, display_name, config_dir)
}

pub(crate) fn push_managed_claude_profile(
    provider: &mut ProviderConfig,
    profile_id: String,
    display_name: Option<String>,
    config_dir: PathBuf,
) -> anyhow::Result<ClaudeLoginTarget> {
    let keychain_account = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    let owns_default_claude_activity = !provider
        .profiles
        .iter()
        .any(|profile| profile.enabled && !profile.deleted);
    std::fs::create_dir_all(&config_dir)?;
    let keychain_service = keychain_service_for_config_dir(&config_dir);
    provider.profiles.push(ProviderProfileConfig {
        id: Some(profile_id.clone()),
        display_name: display_name.clone(),
        keychain_account: Some(keychain_account),
        keychain_service: Some(keychain_service),
        credentials_file: Some(config_dir.join(".credentials.json")),
        claude_config_dir: Some(config_dir.clone()),
        cli_enabled: Some(true),
        project_roots: vec![config_dir.join("projects")],
        owns_default_claude_activity,
        ..ProviderProfileConfig::default()
    });
    Ok(ClaudeLoginTarget {
        profile_id,
        display_name,
        config_dir: Some(config_dir),
    })
}

pub(crate) fn pending_claude_profile(
    provider: &ProviderConfig,
    connected_profiles: &BTreeSet<String>,
) -> Option<ClaudeLoginTarget> {
    provider
        .profiles
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, profile)| {
            if !profile.enabled || profile.deleted {
                return None;
            }
            let target = claude_login_target(profile, index);
            let config_dir = target.config_dir.as_deref()?;
            (managed_profiles::is_managed_profile(config_dir, CLAUDE_PROVIDER_ID)
                && !connected_profiles.contains(&target.profile_id))
            .then_some(target)
        })
}

fn claude_login_target(profile: &ProviderProfileConfig, index: usize) -> ClaudeLoginTarget {
    let profile_id = profile
        .id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| {
            if index == 0 {
                "default".to_string()
            } else {
                format!("profile-{}", index + 1)
            }
        });
    ClaudeLoginTarget {
        profile_id,
        display_name: profile.display_name.clone(),
        config_dir: profile.claude_config_dir.clone().map(expand_home_path),
    }
}

pub(crate) fn unique_profile_id(
    profiles: &[ProviderProfileConfig],
    display_name: Option<&str>,
) -> String {
    let base = display_name
        .map(slugify_profile_id)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "account".to_string());
    let existing = profiles
        .iter()
        .filter_map(|profile| profile.id.as_deref())
        .collect::<BTreeSet<_>>();
    if !existing.contains(base.as_str()) {
        return base;
    }
    for index in 2.. {
        let candidate = format!("{base}-{index}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("infinite profile id search should always return")
}

fn slugify_profile_id(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}
