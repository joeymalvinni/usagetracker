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

    if account.provider_id.as_str() == OPENCODE_GO_PROVIDER_ID {
        provider.enabled = false;
        provider.workspace_id = None;
        provider.cookie_header = None;
    } else if let Some(profile_id) = account.profile_id.as_deref() {
        if account.provider_id.as_str() == GROK_PROVIDER_ID && profile_id == "default" {
            provider.cookie_header = None;
        }
        let configured_profile = provider
            .profiles
            .iter_mut()
            .enumerate()
            .find(|(index, profile)| {
                if account.provider_id.as_str() == GROK_PROVIDER_ID {
                    crate::providers::grok::normalized_profile_id(profile.id.as_deref(), *index)
                        == profile_id
                } else {
                    profile.id.as_deref() == Some(profile_id)
                }
            })
            .map(|(_, profile)| profile);
        if let Some(profile) = configured_profile {
            let configured_path = match account.provider_id.as_str() {
                CODEX_PROVIDER_ID => profile.codex_home.as_deref(),
                CLAUDE_PROVIDER_ID => profile.claude_config_dir.as_deref(),
                GROK_PROVIDER_ID => profile.grok_home.as_deref(),
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
    profile.grok_home = None;
    profile.cli_enabled = None;
    profile.project_roots.clear();
    profile.owns_default_codex_activity = false;
    profile.owns_default_claude_activity = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use usage_core::{
        default_app_dir, AccountDisplayNameSource, AccountId, NotificationConfig, ProviderId,
    };

    fn account(profile_id: &str) -> Account {
        let now = chrono::Utc::now();
        Account {
            id: AccountId::new(format!("account-{profile_id}")),
            provider_id: ProviderId::new(GROK_PROVIDER_ID),
            external_account_id: format!("user-{profile_id}"),
            profile_id: Some(profile_id.to_string()),
            display_name: None,
            display_name_source: AccountDisplayNameSource::Generated,
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn deleting_one_grok_profile_preserves_the_other() {
        let managed_home = default_app_dir().unwrap().join("profiles/grok/work");
        let config = Config {
            poll_interval_seconds: 300,
            notifications: NotificationConfig::default(),
            providers: BTreeMap::from([(
                GROK_PROVIDER_ID.to_string(),
                crate::config::ProviderConfig {
                    enabled: true,
                    cookie_header: Some("sso=legacy".to_string()),
                    profiles: vec![
                        ProviderProfileConfig {
                            id: Some("default".to_string()),
                            grok_home: dirs::home_dir().map(|home| home.join(".grok")),
                            ..ProviderProfileConfig::default()
                        },
                        ProviderProfileConfig {
                            id: Some("work".to_string()),
                            grok_home: Some(managed_home.clone()),
                            ..ProviderProfileConfig::default()
                        },
                    ],
                    ..crate::config::ProviderConfig::default()
                },
            )]),
            paths: crate::config::Paths {
                config: PathBuf::from("/tmp/config.json"),
                db: PathBuf::from("/tmp/usage.sqlite3"),
                socket: PathBuf::from("/tmp/usage.sock"),
            },
        };

        let plan = plan_deletion(&config, &account("work")).unwrap();
        let provider = &plan.config.providers[GROK_PROVIDER_ID];

        assert!(provider.enabled);
        assert!(provider.profiles[0].enabled);
        assert!(provider.profiles[1].deleted);
        assert_eq!(provider.cookie_header.as_deref(), Some("sso=legacy"));
        assert_eq!(
            plan.managed_profile_path.as_deref(),
            Some(managed_home.as_path())
        );

        let default_plan = plan_deletion(&config, &account("default")).unwrap();
        let provider = &default_plan.config.providers[GROK_PROVIDER_ID];
        assert!(provider.enabled);
        assert!(provider.profiles[0].deleted);
        assert!(provider.profiles[1].enabled);
        assert!(provider.cookie_header.is_none());
        assert!(default_plan.managed_profile_path.is_none());
    }

    #[test]
    fn deleting_grok_profile_matches_its_canonical_id() {
        let managed_home = default_app_dir().unwrap().join("profiles/grok/work");
        let config = Config {
            poll_interval_seconds: 300,
            notifications: NotificationConfig::default(),
            providers: BTreeMap::from([(
                GROK_PROVIDER_ID.to_string(),
                crate::config::ProviderConfig {
                    enabled: true,
                    profiles: vec![ProviderProfileConfig {
                        id: Some(" work ".to_string()),
                        grok_home: Some(managed_home.clone()),
                        ..ProviderProfileConfig::default()
                    }],
                    ..crate::config::ProviderConfig::default()
                },
            )]),
            paths: crate::config::Paths {
                config: PathBuf::from("/tmp/config.json"),
                db: PathBuf::from("/tmp/usage.sqlite3"),
                socket: PathBuf::from("/tmp/usage.sock"),
            },
        };

        let plan = plan_deletion(&config, &account("work")).unwrap();

        assert_eq!(plan.config.providers[GROK_PROVIDER_ID].profiles.len(), 1);
        assert!(plan.config.providers[GROK_PROVIDER_ID].profiles[0].deleted);
        assert_eq!(
            plan.managed_profile_path.as_deref(),
            Some(managed_home.as_path())
        );
    }
}
