use std::path::PathBuf;

use usage_core::Account;

use crate::{
    config::{Config, ProviderProfileConfig},
    providers::{
        claude::PROVIDER_ID as CLAUDE_PROVIDER_ID, codex::PROVIDER_ID as CODEX_PROVIDER_ID,
        grok::PROVIDER_ID as GROK_PROVIDER_ID, opencode::OPENCODE_GO_PROVIDER_ID,
        paths::expand_home_path,
    },
    runtime::managed_profiles,
};

pub(crate) struct AccountDeletionPlan {
    pub(crate) config: Config,
    pub(crate) managed_profile_path: Option<PathBuf>,
}

/// Prepares the configuration side of an account deletion without mutating
/// shared state. Account display/visibility/collection fields intentionally do
/// not live in provider config; SQLite is their sole authority.
pub(crate) fn plan_deletion(
    current: &Config,
    account: &Account,
) -> anyhow::Result<AccountDeletionPlan> {
    let mut config = current.clone();
    let provider = config
        .providers
        .entry(account.provider_id.as_str().to_string())
        .or_default();
    let mut managed_profile_path = None;

    if matches!(
        account.provider_id.as_str(),
        OPENCODE_GO_PROVIDER_ID | GROK_PROVIDER_ID
    ) {
        provider.enabled = false;
        provider.workspace_id = None;
        provider.cookie_header = None;
    } else if let Some(profile_id) = account.profile_id.as_deref() {
        if let Some(profile) = provider
            .profiles
            .iter_mut()
            .find(|profile| profile.id.as_deref() == Some(profile_id))
        {
            let configured_path = match account.provider_id.as_str() {
                CODEX_PROVIDER_ID => profile.codex_home.as_deref(),
                CLAUDE_PROVIDER_ID => profile.claude_config_dir.as_deref(),
                _ => None,
            }
            .map(expand_home_path);
            managed_profile_path = managed_profiles::deletion_candidate(
                account.provider_id.as_str(),
                profile_id,
                configured_path.as_deref(),
            )?;
            tombstone(profile);
        } else {
            provider.profiles.push(ProviderProfileConfig {
                id: Some(profile_id.to_string()),
                enabled: false,
                deleted: true,
                ..ProviderProfileConfig::default()
            });
        }
        if !provider
            .profiles
            .iter()
            .any(|profile| profile.enabled && !profile.deleted)
        {
            provider.enabled = false;
        }
    }

    Ok(AccountDeletionPlan {
        config,
        managed_profile_path,
    })
}

fn tombstone(profile: &mut ProviderProfileConfig) {
    profile.enabled = false;
    profile.deleted = true;
    profile.display_name = None;
    profile.auth_path = None;
    profile.codex_home = None;
    profile.keychain_account = None;
    profile.keychain_service = None;
    profile.credentials_file = None;
    profile.claude_config_dir = None;
    profile.cli_enabled = None;
    profile.project_roots.clear();
    profile.owns_default_codex_activity = false;
    profile.owns_default_claude_activity = false;
}
