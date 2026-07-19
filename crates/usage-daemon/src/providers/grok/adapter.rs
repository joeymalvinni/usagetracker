use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use tracing::info;
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ProviderActionResponse, ProviderId,
};

use crate::{
    config::ProviderConfig,
    providers::{launchers, ProviderCollector},
    runtime::provider_adapter::{
        plan_profile_deletion, AccountDeletionPlan, AddAccountHandler, DeleteHandler,
        ExecutionPolicy, LocalUsagePathMatcher, LocalUsageWatch, ProviderAdapter, ProviderManifest,
        ProviderRuntime, RepairHandler,
    },
};

use super::profile_service::{
    ensure_login_profile, select_browser_login_target, select_login_target, GrokLoginTarget,
};
use super::{
    clear_cached_cookie_cache, find_grok_binary, normalized_profile_id, settings, GrokCollector,
    PROVIDER_ID,
};

pub(crate) static ADAPTER: GrokAdapter = GrokAdapter;

pub(crate) struct GrokAdapter;

impl ProviderAdapter for GrokAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: PROVIDER_ID,
            display_name: "Grok",
            minimum_refresh_interval_seconds: 60,
            default_visible: false,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(Duration::from_secs(30), Duration::from_secs(60), 4)
    }

    fn provider_setting_keys(&self) -> &'static [&'static str] {
        &["cookie_header", "source_mode"]
    }

    fn profile_setting_keys(&self) -> &'static [&'static str] {
        &["grok_home"]
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn local_usage_watch(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Option<LocalUsageWatch>> {
        let roots = super::profile::resolve(config)?
            .into_iter()
            .map(|profile| profile.grok_home.join("sessions"))
            .collect::<Vec<_>>();
        Ok(Some(LocalUsageWatch::new(
            roots,
            [LocalUsagePathMatcher::file_name("signals.json")],
            Duration::from_secs(60),
        )))
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(GrokCollector::new(config.clone())?))
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

#[async_trait]
impl AddAccountHandler for GrokAdapter {
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
        let binary = find_grok_binary();
        if binary.is_none() && !connected_profiles.is_empty() {
            anyhow::bail!("adding another Grok account requires the Grok Build CLI");
        }
        let target = runtime
            .mutate_config(|config| {
                let provider = config.providers.entry(PROVIDER_ID.to_string()).or_default();
                provider.enabled = true;
                if binary.is_some() {
                    select_login_target(provider, &connected_profiles, display_name.clone())
                } else {
                    select_browser_login_target(provider)
                }
            })
            .await?;

        let authentication_url = if let Some(binary) = binary {
            let login = launchers::launch_grok_login(&binary, &target.grok_home)?;
            let authentication_url = login.authentication_url.clone();
            launchers::monitor_login(
                login.child,
                runtime.refresh(),
                PROVIDER_ID,
                Some(target.profile_id.clone()),
            );
            authentication_url
        } else {
            let url = "https://grok.com/?_s=usage";
            launchers::open_url(url)?;
            Some(url.to_string())
        };
        info!(
            provider_id = PROVIDER_ID,
            profile_id = target.profile_id.as_str(),
            profile_path = %target.grok_home.display(),
            "provider account login launched"
        );
        Ok(AddProviderAccountResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            profile_id: target.profile_id,
            display_name: target.display_name,
            profile_path: target.grok_home.display().to_string(),
            authentication_url,
        })
    }
}

#[async_trait]
impl RepairHandler for GrokAdapter {
    async fn repair(
        &self,
        runtime: ProviderRuntime<'_>,
        account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
        let (message, authentication_url) = if let Some(binary) = find_grok_binary() {
            let target = prepare_login_profile(runtime, account_id.as_ref()).await?;
            if target.profile_id == "default" {
                clear_cached_cookie_cache().await?;
            }
            let login = launchers::launch_grok_login(&binary, &target.grok_home)?;
            let authentication_url = login.authentication_url.clone();
            launchers::monitor_login(
                login.child,
                runtime.refresh(),
                PROVIDER_ID,
                Some(target.profile_id),
            );
            (
                "Finish signing in to Grok in your browser. UsageTracker will refresh automatically."
                    .to_string(),
                authentication_url,
            )
        } else {
            let requested_profile = match account_id.as_ref() {
                Some(id) => runtime
                    .storage()
                    .account(id)
                    .await?
                    .and_then(|account| account.profile_id),
                None => None,
            };
            if requested_profile
                .as_deref()
                .is_some_and(|id| id != "default")
            {
                anyhow::bail!("reconnecting this Grok account requires the Grok Build CLI");
            }
            clear_cached_cookie_cache().await?;
            let url = "https://grok.com/?_s=usage";
            launchers::open_url(url)?;
            (
                "Grok opened in your browser. Sign in there, or install Grok Build for CLI billing."
                    .to_string(),
                Some(url.to_string()),
            )
        };
        Ok(ProviderActionResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            message,
            authentication_url,
        })
    }
}

#[async_trait]
impl DeleteHandler for GrokAdapter {
    fn plan_deletion(
        &self,
        config: &crate::config::Config,
        account: &Account,
    ) -> anyhow::Result<AccountDeletionPlan> {
        let profile_id = account.profile_id.as_deref();
        let mut plan = plan_profile_deletion(
            config,
            account,
            |index, profile| {
                normalized_profile_id(profile.id.as_deref(), index)
                    == profile_id.unwrap_or_default()
            },
            |profile| Ok(settings::profile(profile)?.grok_home),
        )?;
        if profile_id == Some("default") {
            let provider = plan
                .config
                .providers
                .entry(PROVIDER_ID.to_string())
                .or_default();
            settings::update_provider(provider, |settings| settings.cookie_header = None)?;
        }
        Ok(plan)
    }

    async fn cleanup_before_delete(&self, account: &Account) -> anyhow::Result<()> {
        if account.profile_id.as_deref() == Some("default") {
            clear_cached_cookie_cache().await?;
        }
        Ok(())
    }
}

async fn prepare_login_profile(
    runtime: ProviderRuntime<'_>,
    account_id: Option<&AccountId>,
) -> anyhow::Result<GrokLoginTarget> {
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
            ensure_login_profile(provider, requested_profile_id.as_deref())
        })
        .await
}
