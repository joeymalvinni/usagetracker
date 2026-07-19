use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use tracing::info;
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ProviderActionResponse, ProviderId,
};

use crate::{
    config::{ProviderConfig, ProviderProfileConfig},
    providers::{
        launchers,
        paths::expand_home_path,
        profile_service::{
            codex_profile_home, default_codex_profile, pending_codex_profile, unique_profile_id,
        },
        ProviderCollector,
    },
    runtime::provider_adapter::{
        plan_profile_deletion, AccountDeletionPlan, AddAccountHandler, DeleteHandler,
        ExecutionPolicy, LocalUsagePathMatcher, LocalUsageWatch, ProviderAdapter, ProviderManifest,
        ProviderRuntime, RepairHandler,
    },
};

use super::{settings, CodexCollector, PROVIDER_ID};

pub(crate) static ADAPTER: CodexAdapter = CodexAdapter;

pub(crate) struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: PROVIDER_ID,
            display_name: "Codex",
            minimum_refresh_interval_seconds: 60,
            default_visible: true,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(Duration::from_secs(30), Duration::from_secs(60), 4)
    }

    fn profile_setting_keys(&self) -> &'static [&'static str] {
        &["auth_path", "codex_home", "owns_default_codex_activity"]
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn local_usage_watch(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Option<LocalUsageWatch>> {
        let mut roots = default_local_home()
            .map(|home| vec![home.join("sessions")])
            .unwrap_or_default();
        for profile in config
            .profiles
            .iter()
            .filter(|profile| profile.enabled && !profile.deleted)
        {
            if let Some(home) = settings::profile(profile)?.codex_home {
                roots.push(expand_home_path(home).join("sessions"));
            }
        }
        roots.sort();
        roots.dedup();
        Ok(Some(
            LocalUsageWatch::new(
                roots,
                [LocalUsagePathMatcher::extension("jsonl")],
                Duration::from_secs(60),
            )
            .with_timing(Duration::from_secs(30), Duration::from_secs(60)),
        ))
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(CodexCollector::new(config.clone())?))
    }

    fn migrate_config(
        &self,
        config: &mut ProviderConfig,
        discover_local_activity_owners: bool,
    ) -> anyhow::Result<bool> {
        if !discover_local_activity_owners {
            return Ok(false);
        }
        let Some(local_home) = default_local_home() else {
            return Ok(false);
        };
        assign_default_activity_owner_for_home(config, &local_home)
    }

    fn add_account_handler(&self) -> Option<&dyn AddAccountHandler> {
        Some(self)
    }

    fn repair_handler(&self) -> Option<&dyn RepairHandler> {
        Some(self)
    }

    fn delete_handler(&self) -> Option<&dyn DeleteHandler> {
        Some(self)
    }
}

fn default_local_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(expand_home_path)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}

pub(crate) fn assign_default_activity_owner_for_home(
    config: &mut ProviderConfig,
    local_home: &Path,
) -> anyhow::Result<bool> {
    let Some(local_account_id) = account_id_from_auth(&local_home.join("auth.json")) else {
        return Ok(false);
    };
    let decoded = config
        .profiles
        .iter()
        .map(settings::profile)
        .collect::<anyhow::Result<Vec<_>>>()?;
    if config
        .profiles
        .iter()
        .zip(&decoded)
        .any(|(profile, settings)| {
            profile.enabled && !profile.deleted && settings.owns_default_codex_activity
        })
    {
        return Ok(false);
    }
    if config
        .profiles
        .iter()
        .zip(&decoded)
        .filter(|(profile, _)| profile.enabled && !profile.deleted)
        .any(|(_, settings)| {
            settings
                .codex_home
                .as_deref()
                .map(expand_home_path)
                .unwrap_or_else(|| local_home.to_path_buf())
                == local_home
        })
    {
        return Ok(false);
    }
    let matching = config
        .profiles
        .iter()
        .zip(&decoded)
        .enumerate()
        .filter(|(_, (profile, _))| profile.enabled && !profile.deleted)
        .filter_map(|(index, (_, settings))| {
            let home = settings
                .codex_home
                .as_deref()
                .map(expand_home_path)
                .unwrap_or_else(|| local_home.to_path_buf());
            if home == local_home {
                return None;
            }
            let auth_path = settings
                .auth_path
                .as_deref()
                .map(expand_home_path)
                .unwrap_or_else(|| home.join("auth.json"));
            (account_id_from_auth(&auth_path).as_deref() == Some(local_account_id.as_str()))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Ok(false);
    }
    settings::update_profile(&mut config.profiles[matching[0]], |settings| {
        settings.owns_default_codex_activity = true;
    })?;
    Ok(true)
}

fn account_id_from_auth(path: &Path) -> Option<String> {
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value
        .get("tokens")?
        .get("account_id")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[async_trait]
impl AddAccountHandler for CodexAdapter {
    async fn add_account(
        &self,
        runtime: ProviderRuntime<'_>,
        display_name: Option<String>,
    ) -> anyhow::Result<AddProviderAccountResponse> {
        let (profile_id, profile_path, profile_name) = runtime
            .mutate_config(|config| {
                let provider = config.providers.entry(PROVIDER_ID.to_string()).or_default();
                provider.enabled = true;
                if provider.profiles.is_empty() {
                    provider.profiles.push(default_codex_profile()?);
                }
                if let Some(pending) = pending_codex_profile(provider) {
                    return Ok(pending);
                }
                let profile_id = unique_profile_id(&provider.profiles, display_name.as_deref());
                let profile_path = codex_profile_home(&profile_id)?;
                std::fs::create_dir_all(&profile_path)?;
                let mut profile = ProviderProfileConfig {
                    id: Some(profile_id.clone()),
                    display_name: display_name.clone(),
                    ..ProviderProfileConfig::default()
                };
                settings::update_profile(&mut profile, |settings| {
                    settings.codex_home = Some(profile_path.clone());
                })?;
                provider.profiles.push(profile);
                Ok((profile_id, profile_path, display_name.clone()))
            })
            .await?;

        let login = launchers::launch_codex_login(&profile_path)?;
        let authentication_url = login.authentication_url.clone();
        launchers::monitor_login(
            login.child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(profile_id.clone()),
        );
        info!(
            provider_id = PROVIDER_ID,
            profile_id = profile_id.as_str(),
            profile_path = %profile_path.display(),
            "provider account login launched"
        );
        Ok(AddProviderAccountResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            profile_id,
            display_name: profile_name,
            profile_path: profile_path.display().to_string(),
            authentication_url,
        })
    }
}

#[async_trait]
impl RepairHandler for CodexAdapter {
    async fn repair(
        &self,
        runtime: ProviderRuntime<'_>,
        account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
        let config = runtime.config().await;
        let account = match account_id.as_ref() {
            Some(id) => runtime.storage().account(id).await?,
            None => None,
        };
        let requested_profile = account
            .as_ref()
            .and_then(|account| account.profile_id.as_deref());
        let profile = config.providers.get(PROVIDER_ID).and_then(|provider| {
            requested_profile
                .and_then(|id| {
                    provider
                        .profiles
                        .iter()
                        .find(|profile| profile.id.as_deref() == Some(id))
                })
                .or_else(|| provider.profiles.first())
        });
        let home = profile
            .and_then(|profile| settings::profile(profile).ok()?.codex_home)
            .map(expand_home_path)
            .unwrap_or_else(|| {
                default_codex_profile()
                    .ok()
                    .and_then(|profile| settings::profile(&profile).ok()?.codex_home)
                    .unwrap_or_default()
            });
        let profile_id = profile
            .and_then(|profile| profile.id.clone())
            .unwrap_or_else(|| "default".to_string());
        let login = launchers::launch_codex_login(&home)?;
        let authentication_url = login.authentication_url.clone();
        launchers::monitor_login(
            login.child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(profile_id),
        );
        Ok(ProviderActionResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            message: "Finish signing in to Codex in your browser. UsageTracker will refresh automatically."
                .to_string(),
            authentication_url,
        })
    }
}

#[async_trait]
impl DeleteHandler for CodexAdapter {
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
            |profile| Ok(settings::profile(profile)?.codex_home),
        )
    }
}
