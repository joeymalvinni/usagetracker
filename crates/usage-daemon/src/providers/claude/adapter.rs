use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use tracing::info;
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ProviderActionResponse, ProviderId,
};

use crate::{
    config::ProviderConfig,
    providers::{
        launchers,
        paths::expand_home_path,
        profile_service::{
            create_managed_claude_profile, ensure_claude_login_profile, pending_claude_profile,
            ClaudeLoginTarget,
        },
        ProviderCollector,
    },
    runtime::provider_adapter::{
        plan_profile_deletion, AccountDeletionPlan, AddAccountHandler, DeleteHandler,
        ExecutionPolicy, LaunchHandler, LocalUsageWatch, ProviderAdapter, ProviderManifest,
        ProviderRuntime, RepairHandler,
    },
};

use super::{settings, ClaudeCollector, PROVIDER_ID};

pub(crate) static ADAPTER: ClaudeAdapter = ClaudeAdapter;

pub(crate) struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: PROVIDER_ID,
            display_name: "Claude",
            minimum_refresh_interval_seconds: 60,
            default_visible: false,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(Duration::from_secs(30), Duration::from_secs(75), 2)
    }

    fn profile_setting_keys(&self) -> &'static [&'static str] {
        &[
            "keychain_account",
            "keychain_service",
            "credentials_file",
            "claude_config_dir",
            "cli_enabled",
            "project_roots",
            "owns_default_claude_activity",
        ]
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn local_usage_watch(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Option<LocalUsageWatch>> {
        let mut roots = Vec::new();
        if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
            roots.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| PathBuf::from(value).join("projects")),
            );
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".config/claude/projects"));
            roots.push(home.join(".claude/projects"));
        }

        let managed_root =
            usage_core::default_app_dir().map(|root| root.join("profiles").join(PROVIDER_ID));
        if let Some(root) = managed_root.as_ref() {
            roots.push(root.clone());
        }
        for profile in config
            .profiles
            .iter()
            .filter(|profile| profile.enabled && !profile.deleted)
        {
            let settings = settings::profile(profile)?;
            let configured = if settings.project_roots.is_empty() {
                settings
                    .claude_config_dir
                    .as_ref()
                    .map(|root| vec![expand_home_path(root).join("projects")])
                    .unwrap_or_default()
            } else {
                settings
                    .project_roots
                    .iter()
                    .map(expand_home_path)
                    .collect()
            };
            roots.extend(configured.into_iter().filter(|root| {
                managed_root
                    .as_ref()
                    .is_none_or(|managed| !root.starts_with(managed))
            }));
        }
        roots.sort();
        roots.dedup();
        Ok(Some(LocalUsageWatch {
            roots,
            minimum_refresh_interval: Duration::from_secs(60),
        }))
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(ClaudeCollector::new(config.clone())?))
    }

    fn migrate_config(
        &self,
        config: &mut ProviderConfig,
        discover_local_activity_owners: bool,
    ) -> anyhow::Result<bool> {
        if discover_local_activity_owners {
            assign_default_activity_owner(config)
        } else {
            Ok(false)
        }
    }

    fn add_account_handler(&self) -> Option<&dyn AddAccountHandler> {
        Some(self)
    }

    fn repair_handler(&self) -> Option<&dyn RepairHandler> {
        Some(self)
    }

    fn launch_handler(&self) -> Option<&dyn LaunchHandler> {
        Some(self)
    }

    fn delete_handler(&self) -> Option<&dyn DeleteHandler> {
        Some(self)
    }
}

pub(crate) fn assign_default_activity_owner(config: &mut ProviderConfig) -> anyhow::Result<bool> {
    let active = config
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if active.len() != 1 {
        return Ok(false);
    }
    let profile_settings = settings::profile(&config.profiles[active[0]])?;
    if profile_settings.owns_default_claude_activity
        || profile_settings.claude_config_dir.is_none()
        || profile_settings.project_roots.iter().any(|root| {
            let value = root.to_string_lossy();
            value.ends_with("/.claude/projects") || value.ends_with("/.config/claude/projects")
        })
    {
        return Ok(false);
    }
    settings::update_profile(&mut config.profiles[active[0]], |settings| {
        settings.owns_default_claude_activity = true;
    })?;
    Ok(true)
}

#[async_trait]
impl AddAccountHandler for ClaudeAdapter {
    async fn add_account(
        &self,
        runtime: ProviderRuntime<'_>,
        display_name: Option<String>,
    ) -> anyhow::Result<AddProviderAccountResponse> {
        let connected_profiles = runtime
            .storage()
            .accounts()
            .await?
            .into_iter()
            .filter(|account| account.provider_id.as_str() == PROVIDER_ID)
            .filter_map(|account| account.profile_id)
            .collect::<BTreeSet<_>>();
        let target = runtime
            .mutate_config(|config| {
                let provider = config.providers.entry(PROVIDER_ID.to_string()).or_default();
                provider.enabled = true;
                pending_claude_profile(provider, &connected_profiles)
                    .map(Ok)
                    .unwrap_or_else(|| {
                        create_managed_claude_profile(provider, display_name.clone())
                    })
            })
            .await?;

        let profile_path = target
            .config_dir
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed Claude profile is missing its config path"))?;
        let child = launchers::launch_claude_login(Some(&profile_path))?;
        launchers::monitor_login(
            child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(target.profile_id.clone()),
        );
        info!(
            provider_id = PROVIDER_ID,
            profile_id = target.profile_id.as_str(),
            profile_path = %profile_path.display(),
            "provider account login launched"
        );
        Ok(AddProviderAccountResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            profile_id: target.profile_id,
            display_name: target.display_name,
            profile_path: profile_path.display().to_string(),
        })
    }
}

#[async_trait]
impl RepairHandler for ClaudeAdapter {
    async fn repair(
        &self,
        runtime: ProviderRuntime<'_>,
        account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
        let target = prepare_login_profile(runtime, account_id.as_ref()).await?;
        let child = launchers::launch_claude_login(target.config_dir.as_deref())?;
        launchers::monitor_login(
            child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(target.profile_id),
        );
        Ok(ProviderActionResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            message: "Finish signing in to Claude in your browser. UsageTracker will refresh automatically."
                .to_string(),
        })
    }
}

#[async_trait]
impl LaunchHandler for ClaudeAdapter {
    async fn launch(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<ProviderActionResponse> {
        if !account.collection_enabled {
            anyhow::bail!("enable Claude account tracking before opening a profile session");
        }
        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = runtime.config().await;
        let provider = config
            .providers
            .get(PROVIDER_ID)
            .ok_or_else(|| anyhow::anyhow!("Claude is not configured"))?;
        let config_dir = if provider.profiles.is_empty() && profile_id == "default" {
            None
        } else {
            let profile = provider
                .profiles
                .iter()
                .find(|profile| {
                    profile.enabled && !profile.deleted && profile.id.as_deref() == Some(profile_id)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Claude profile {profile_id} is no longer configured")
                })?;
            settings::profile(profile)?
                .claude_config_dir
                .map(expand_home_path)
        };
        let launcher =
            launchers::write_claude_profile_launcher(&account.id, config_dir.as_deref())?;
        launchers::open_terminal(&launcher)?;
        Ok(ProviderActionResponse {
            provider_id: account.provider_id,
            message: format!(
                "Opened a Claude session for {}. Activity from this terminal stays with this profile.",
                account.display_name.as_deref().unwrap_or(profile_id)
            ),
        })
    }
}

#[async_trait]
impl DeleteHandler for ClaudeAdapter {
    fn plan_deletion(
        &self,
        config: &crate::config::Config,
        account: &Account,
    ) -> anyhow::Result<AccountDeletionPlan> {
        let profile_id = account.profile_id.as_deref();
        plan_profile_deletion(
            config,
            account,
            |_, profile| profile.id.as_deref() == profile_id,
            |profile| Ok(settings::profile(profile)?.claude_config_dir),
        )
    }
}

async fn prepare_login_profile(
    runtime: ProviderRuntime<'_>,
    account_id: Option<&AccountId>,
) -> anyhow::Result<ClaudeLoginTarget> {
    let requested_profile_id = match account_id {
        Some(account_id) => runtime
            .storage()
            .account(account_id)
            .await?
            .and_then(|account| account.profile_id),
        None => None,
    };
    runtime
        .mutate_config(|config| {
            let provider = config.providers.entry(PROVIDER_ID.to_string()).or_default();
            provider.enabled = true;
            ensure_claude_login_profile(provider, requested_profile_id.as_deref())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProfileConfig;

    #[test]
    fn local_watch_roots_keep_managed_and_manual_profiles_separate() {
        let profile =
            |id: &str, enabled: bool, config_dir: Option<PathBuf>, roots: Vec<PathBuf>| {
                let mut profile = ProviderProfileConfig {
                    id: Some(id.to_string()),
                    enabled,
                    ..ProviderProfileConfig::default()
                };
                settings::update_profile(&mut profile, |settings| {
                    settings.claude_config_dir = config_dir;
                    settings.project_roots = roots;
                })
                .unwrap();
                profile
            };
        let config = ProviderConfig {
            enabled: true,
            profiles: vec![
                profile(
                    "managed",
                    true,
                    usage_core::default_app_dir().map(|root| root.join("profiles/claude/managed")),
                    Vec::new(),
                ),
                profile(
                    "manual",
                    true,
                    None,
                    vec![PathBuf::from("/tmp/manual-claude/projects")],
                ),
                profile(
                    "disabled",
                    false,
                    None,
                    vec![PathBuf::from("/tmp/disabled-claude/projects")],
                ),
            ],
            ..ProviderConfig::default()
        };

        let watch = ADAPTER.local_usage_watch(&config).unwrap().unwrap();

        assert!(watch
            .roots
            .contains(&PathBuf::from("/tmp/manual-claude/projects")));
        assert!(!watch
            .roots
            .contains(&PathBuf::from("/tmp/disabled-claude/projects")));
        if let Some(managed) =
            usage_core::default_app_dir().map(|root| root.join("profiles").join(PROVIDER_ID))
        {
            assert!(watch.roots.contains(&managed));
            assert!(!watch.roots.contains(&managed.join("managed/projects")));
        }
    }
}
