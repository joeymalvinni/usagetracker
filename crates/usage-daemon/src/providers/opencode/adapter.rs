use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use usage_core::{
    Account, AccountId, ProviderActionResponse, ProviderId, ProviderProfileResponse,
    ProviderSetupField, ProviderSetupFieldKind, ProviderSetupResponse,
};

use crate::{
    config::ProviderConfig,
    providers::{launchers, ProviderCollector},
    runtime::provider_adapter::{
        AccountDeletionPlan, DeleteHandler, ExecutionPolicy, LocalUsagePathMatcher,
        LocalUsageWatch, ProviderAdapter, ProviderManifest, ProviderRuntime, RepairHandler,
        SetupHandler,
    },
};

use super::{clear_cached_cookie_cache, settings, OpenCodeCollector, OPENCODE_GO_PROVIDER_ID};

pub(crate) static ADAPTER: OpenCodeAdapter = OpenCodeAdapter;

pub(crate) struct OpenCodeAdapter;

impl ProviderAdapter for OpenCodeAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: OPENCODE_GO_PROVIDER_ID,
            display_name: "OpenCode Go",
            minimum_refresh_interval_seconds: 60,
            default_visible: false,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(Duration::from_secs(30), Duration::from_secs(60), 1)
    }

    fn provider_setting_keys(&self) -> &'static [&'static str] {
        &["cookie_header", "workspace_id"]
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn local_usage_watch(
        &self,
        _config: &ProviderConfig,
    ) -> anyhow::Result<Option<LocalUsageWatch>> {
        let Some(home) = dirs::home_dir() else {
            return Ok(None);
        };
        Ok(Some(LocalUsageWatch::new(
            vec![home.join(".local/share/opencode")],
            [
                LocalUsagePathMatcher::file_name("opencode.db"),
                LocalUsagePathMatcher::suffix(".db-wal"),
            ],
            Duration::from_secs(60),
        )))
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(OpenCodeCollector::new(config.clone())?))
    }

    fn repair_handler(&self) -> Option<&dyn RepairHandler> {
        Some(self)
    }

    fn setup_handler(&self) -> Option<&dyn SetupHandler> {
        Some(self)
    }

    fn delete_handler(&self) -> Option<&dyn DeleteHandler> {
        Some(self)
    }
}

#[async_trait]
impl RepairHandler for OpenCodeAdapter {
    async fn repair(
        &self,
        _runtime: ProviderRuntime<'_>,
        _account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
        clear_cached_cookie_cache().await?;
        launchers::open_url("https://opencode.ai")?;
        Ok(ProviderActionResponse {
            provider_id: ProviderId::new(OPENCODE_GO_PROVIDER_ID),
            message:
                "OpenCode opened in your browser. Sign in, then discover workspaces and refresh."
                    .to_string(),
            authentication_url: Some("https://opencode.ai".to_string()),
        })
    }
}

#[async_trait]
impl SetupHandler for OpenCodeAdapter {
    async fn get_setup(
        &self,
        runtime: ProviderRuntime<'_>,
    ) -> anyhow::Result<ProviderSetupResponse> {
        let config = runtime.config().await;
        let provider = config
            .providers
            .get(OPENCODE_GO_PROVIDER_ID)
            .cloned()
            .unwrap_or_default();
        let profiles = provider
            .profiles
            .iter()
            .filter(|profile| !profile.deleted)
            .enumerate()
            .map(|(index, profile)| ProviderProfileResponse {
                id: profile
                    .id
                    .clone()
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or_else(|| format!("profile-{}", index + 1)),
                display_name: profile.display_name.clone(),
                enabled: profile.enabled,
            })
            .collect();
        let collector = OpenCodeCollector::new(provider.clone())?;
        let (mut workspace_options, discovery_error) =
            match collector.discover_workspace_options().await {
                Ok(options) => (options, None),
                Err(err) => (Vec::new(), Some(err.short_message().to_string())),
            };
        let provider_settings = settings::provider(&provider)?;
        if let Some(selected) = provider_settings.workspace_id.as_deref() {
            if !workspace_options.iter().any(|option| option == selected) {
                workspace_options.insert(0, selected.to_string());
            }
        }
        let selected_workspace_id = provider_settings.workspace_id;
        Ok(ProviderSetupResponse {
            provider_id: ProviderId::new(OPENCODE_GO_PROVIDER_ID),
            profiles,
            fields: vec![ProviderSetupField {
                key: "workspace_id".to_string(),
                label: "Workspace".to_string(),
                kind: ProviderSetupFieldKind::Select,
                value: selected_workspace_id.clone(),
                options: workspace_options.clone(),
                required: false,
                help_text: Some(
                    "Choose a workspace, or clear the value to use automatic discovery."
                        .to_string(),
                ),
            }],
            selected_workspace_id,
            workspace_options,
            discovery_error,
        })
    }

    async fn update_setup(
        &self,
        runtime: ProviderRuntime<'_>,
        mut values: BTreeMap<String, Option<String>>,
    ) -> anyhow::Result<ProviderSetupResponse> {
        let workspace_id = values
            .remove("workspace_id")
            .ok_or_else(|| anyhow::anyhow!("missing setup setting: workspace_id"))?;
        if !values.is_empty() {
            anyhow::bail!(
                "unsupported OpenCode setup settings: {}",
                values.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        let workspace_id = workspace_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if workspace_id
            .as_deref()
            .is_some_and(|value| !value.starts_with("wrk_"))
        {
            anyhow::bail!("OpenCode workspace id must start with wrk_");
        }
        runtime
            .mutate_config(|config| {
                let provider = config
                    .providers
                    .entry(OPENCODE_GO_PROVIDER_ID.to_string())
                    .or_default();
                settings::update_provider(provider, |settings| {
                    settings.workspace_id = workspace_id;
                })
            })
            .await?;
        self.get_setup(runtime).await
    }
}

#[async_trait]
impl DeleteHandler for OpenCodeAdapter {
    fn plan_deletion(
        &self,
        config: &crate::config::Config,
        account: &Account,
    ) -> anyhow::Result<AccountDeletionPlan> {
        let mut config = config.clone();
        let provider = config
            .providers
            .entry(account.provider_id.as_str().to_string())
            .or_default();
        provider.enabled = false;
        settings::update_provider(provider, |settings| {
            settings.workspace_id = None;
            settings.cookie_header = None;
        })?;
        Ok(AccountDeletionPlan {
            config,
            managed_profile_path: None,
        })
    }

    async fn cleanup_before_delete(&self, _account: &Account) -> anyhow::Result<()> {
        clear_cached_cookie_cache().await
    }
}
