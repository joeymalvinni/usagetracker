use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use usage_core::{
    Account, AccountId, AddProviderAccountResponse, ProviderActionResponse, ProviderCapabilities,
    ProviderDescriptor, ProviderId, ProviderProfileResponse, ProviderSetupResponse,
    ProviderSignInAction,
};

use crate::{
    config::{Config, ProviderConfig, ProviderProfileConfig},
    daemon::DaemonRuntime,
    polling::RefreshCoordinator,
    providers::{paths::expand_home_path, ProviderCollector},
    runtime::managed_profiles,
    storage::Storage,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecutionPolicy {
    pub(crate) discovery_timeout: Duration,
    pub(crate) collection_timeout: Duration,
    pub(crate) max_parallel_accounts: usize,
}

impl ExecutionPolicy {
    pub(crate) const fn new(
        discovery_timeout: Duration,
        collection_timeout: Duration,
        max_parallel_accounts: usize,
    ) -> Self {
        Self {
            discovery_timeout,
            collection_timeout,
            max_parallel_accounts,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderManifest {
    pub(crate) id: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) minimum_refresh_interval_seconds: u64,
    pub(crate) default_visible: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalUsageWatch {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) matchers: Vec<LocalUsagePathMatcher>,
    pub(crate) debounce: Duration,
    pub(crate) maximum_latency: Duration,
    pub(crate) minimum_refresh_interval: Duration,
}

impl LocalUsageWatch {
    pub(crate) fn new(
        roots: Vec<PathBuf>,
        matchers: impl IntoIterator<Item = LocalUsagePathMatcher>,
        minimum_refresh_interval: Duration,
    ) -> Self {
        Self {
            roots,
            matchers: matchers.into_iter().collect(),
            debounce: Duration::from_secs(30),
            maximum_latency: Duration::from_secs(60),
            minimum_refresh_interval,
        }
    }

    pub(crate) fn with_timing(mut self, debounce: Duration, maximum_latency: Duration) -> Self {
        self.debounce = debounce;
        self.maximum_latency = maximum_latency;
        self
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.roots.is_empty(), "local usage watch has no roots");
        anyhow::ensure!(
            !self.matchers.is_empty(),
            "local usage watch has no path matchers"
        );
        anyhow::ensure!(!self.debounce.is_zero(), "local usage debounce is zero");
        anyhow::ensure!(
            self.maximum_latency >= self.debounce,
            "local usage maximum latency is shorter than its debounce"
        );
        anyhow::ensure!(
            !self.minimum_refresh_interval.is_zero(),
            "local usage minimum refresh interval is zero"
        );
        for matcher in &self.matchers {
            anyhow::ensure!(!matcher.value().is_empty(), "local usage matcher is empty");
        }
        Ok(())
    }
}

/// Declarative path matching keeps provider-specific file layouts out of the
/// shared watcher while preserving an inspectable, comparable watch config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LocalUsagePathMatcher {
    Extension(String),
    FileName(String),
    Suffix(String),
}

impl LocalUsagePathMatcher {
    pub(crate) fn extension(value: impl Into<String>) -> Self {
        Self::Extension(value.into())
    }

    pub(crate) fn file_name(value: impl Into<String>) -> Self {
        Self::FileName(value.into())
    }

    pub(crate) fn suffix(value: impl Into<String>) -> Self {
        Self::Suffix(value.into())
    }

    pub(crate) fn matches(&self, path: &std::path::Path) -> bool {
        match self {
            Self::Extension(expected) => path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case(expected)),
            Self::FileName(expected) => path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value == expected),
            Self::Suffix(expected) => path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.ends_with(expected)),
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::Extension(value) | Self::FileName(value) | Self::Suffix(value) => value,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProviderRuntime<'a> {
    runtime: &'a DaemonRuntime,
}

impl<'a> ProviderRuntime<'a> {
    pub(crate) fn new(runtime: &'a DaemonRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn storage(self) -> &'a Storage {
        &self.runtime.storage
    }

    pub(crate) fn refresh(self) -> Arc<RefreshCoordinator> {
        self.runtime.refresh.clone()
    }

    pub(crate) async fn config(self) -> Config {
        self.runtime.config_snapshot().await
    }

    pub(crate) async fn mutate_config<T>(
        self,
        mutation: impl FnOnce(&mut Config) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.runtime.mutate_config(mutation).await
    }
}

pub(crate) struct AccountDeletionPlan {
    pub(crate) config: Config,
    pub(crate) managed_profile_path: Option<PathBuf>,
}

impl AccountDeletionPlan {
    pub(crate) fn unchanged(config: &Config) -> Self {
        Self {
            config: config.clone(),
            managed_profile_path: None,
        }
    }
}

pub(crate) fn plan_profile_deletion(
    current: &Config,
    account: &Account,
    profile_matches: impl Fn(usize, &ProviderProfileConfig) -> bool,
    configured_path: impl Fn(&ProviderProfileConfig) -> anyhow::Result<Option<PathBuf>>,
) -> anyhow::Result<AccountDeletionPlan> {
    let mut config = current.clone();
    let provider = config
        .providers
        .entry(account.provider_id.as_str().to_string())
        .or_default();
    let Some(profile_id) = account.profile_id.as_deref() else {
        return Ok(AccountDeletionPlan::unchanged(&config));
    };

    let mut managed_profile_path = None;
    if let Some(profile) = provider
        .profiles
        .iter_mut()
        .enumerate()
        .find(|(index, profile)| profile_matches(*index, profile))
        .map(|(_, profile)| profile)
    {
        let path = configured_path(profile)?.map(expand_home_path);
        managed_profile_path = managed_profiles::deletion_candidate(
            account.provider_id.as_str(),
            profile_id,
            path.as_deref(),
        )?;
        tombstone_profile(profile);
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

    Ok(AccountDeletionPlan {
        config,
        managed_profile_path,
    })
}

fn tombstone_profile(profile: &mut ProviderProfileConfig) {
    profile.enabled = false;
    profile.deleted = true;
    profile.display_name = None;
    profile.clear_settings();
}

#[async_trait]
pub(crate) trait AddAccountHandler: Send + Sync {
    async fn add_account(
        &self,
        runtime: ProviderRuntime<'_>,
        display_name: Option<String>,
        sign_in_action: ProviderSignInAction,
    ) -> anyhow::Result<AddProviderAccountResponse>;
}

#[async_trait]
pub(crate) trait RepairHandler: Send + Sync {
    async fn repair(
        &self,
        runtime: ProviderRuntime<'_>,
        account_id: Option<AccountId>,
        sign_in_action: ProviderSignInAction,
    ) -> anyhow::Result<ProviderActionResponse>;
}

#[async_trait]
pub(crate) trait LaunchHandler: Send + Sync {
    async fn launch(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<ProviderActionResponse>;
}

#[async_trait]
pub(crate) trait SetupHandler: Send + Sync {
    async fn get_setup(
        &self,
        runtime: ProviderRuntime<'_>,
    ) -> anyhow::Result<ProviderSetupResponse>;

    async fn update_setup(
        &self,
        runtime: ProviderRuntime<'_>,
        settings: BTreeMap<String, Option<String>>,
    ) -> anyhow::Result<ProviderSetupResponse>;
}

#[async_trait]
pub(crate) trait DeleteHandler: Send + Sync {
    fn plan_deletion(
        &self,
        config: &Config,
        account: &Account,
    ) -> anyhow::Result<AccountDeletionPlan>;

    async fn cleanup_before_delete(&self, _account: &Account) -> anyhow::Result<()> {
        Ok(())
    }
}

pub(crate) trait ProviderAdapter: Send + Sync {
    fn manifest(&self) -> ProviderManifest;

    fn execution_policy(&self) -> ExecutionPolicy;

    /// Flattened setting keys owned by this adapter. The config loader removes
    /// other keys with a warning so stale configuration cannot prevent startup.
    fn provider_setting_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Flattened per-profile setting keys owned by this adapter.
    fn profile_setting_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Validates every provider-owned setting before any runtime component is
    /// constructed. Adapters must reject misspelled and unsupported keys.
    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()>;

    /// Optional local files whose changes should trigger a provider refresh.
    /// The adapter owns discovery so the shared watcher has no provider IDs,
    /// environment variables, or profile layouts baked into it.
    fn local_usage_watch(
        &self,
        _config: &ProviderConfig,
    ) -> anyhow::Result<Option<LocalUsageWatch>> {
        Ok(None)
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>>;

    /// Applies provider-owned migrations after deserialization. Returning true
    /// asks the shared loader to persist the normalized configuration.
    fn migrate_config(
        &self,
        _config: &mut ProviderConfig,
        _discover_local_activity_owners: bool,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn add_account_handler(&self) -> Option<&dyn AddAccountHandler> {
        None
    }

    fn repair_handler(&self) -> Option<&dyn RepairHandler> {
        None
    }

    fn launch_handler(&self) -> Option<&dyn LaunchHandler> {
        None
    }

    fn setup_handler(&self) -> Option<&dyn SetupHandler> {
        None
    }

    fn setup_summary(&self, config: &ProviderConfig) -> ProviderSetupResponse {
        ProviderSetupResponse {
            provider_id: ProviderId::new(self.manifest().id),
            profiles: config
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
                .collect(),
            fields: Vec::new(),
            selected_workspace_id: None,
            workspace_options: Vec::new(),
            discovery_error: None,
        }
    }

    fn delete_handler(&self) -> Option<&dyn DeleteHandler> {
        None
    }

    fn descriptor(&self) -> ProviderDescriptor {
        let manifest = self.manifest();
        ProviderDescriptor {
            id: ProviderId::new(manifest.id),
            display_name: manifest.display_name.to_string(),
            minimum_refresh_interval_seconds: manifest.minimum_refresh_interval_seconds,
            capabilities: ProviderCapabilities {
                multiple_accounts: self.add_account_handler().is_some(),
                add_account: self.add_account_handler().is_some(),
                repair: self.repair_handler().is_some(),
                launch_account: self.launch_handler().is_some(),
                // `workspace_setup` is a deprecated v3 wire alias. Both fields
                // intentionally derive from the same generic setup handler.
                setup: self.setup_handler().is_some(),
                workspace_setup: self.setup_handler().is_some(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use usage_core::AccountDisplayNameSource;

    #[test]
    fn profile_deletion_propagates_configured_path_decode_failures() {
        let profile = ProviderProfileConfig {
            id: Some("work".to_string()),
            ..ProviderProfileConfig::default()
        };
        let config = Config {
            poll_interval_seconds: 300,
            notifications: Default::default(),
            providers: BTreeMap::from([(
                "codex".to_string(),
                ProviderConfig {
                    enabled: true,
                    profiles: vec![profile],
                    ..ProviderConfig::default()
                },
            )]),
            paths: crate::config::Paths {
                config: PathBuf::from("config.json"),
                db: PathBuf::from("usage.sqlite3"),
                socket: PathBuf::from("usage.sock"),
            },
        };
        let now = Utc::now();
        let account = Account {
            id: AccountId::new("account"),
            provider_id: ProviderId::new("codex"),
            external_account_id: "external".to_string(),
            profile_id: Some("work".to_string()),
            display_name: None,
            display_name_source: AccountDisplayNameSource::Generated,
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: now,
            updated_at: now,
        };

        let error = match plan_profile_deletion(
            &config,
            &account,
            |_, profile| profile.id.as_deref() == Some("work"),
            |_| anyhow::bail!("profile settings could not be decoded"),
        ) {
            Ok(_) => panic!("profile settings decode failure was ignored"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("profile settings could not be decoded"));
    }
}
