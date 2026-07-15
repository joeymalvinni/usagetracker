use std::{collections::BTreeMap, sync::Arc};

use anyhow::Context;
use tracing::warn;
use usage_core::{ProviderDescriptor, ProviderId};

use crate::{
    config::{Config, ProviderConfig},
    providers::{
        claude::adapter::ADAPTER as CLAUDE, codex::adapter::ADAPTER as CODEX,
        grok::adapter::ADAPTER as GROK, opencode::adapter::ADAPTER as OPENCODE_GO,
        ProviderCollector,
    },
    runtime::provider_adapter::{ExecutionPolicy, ProviderAdapter},
};

const PROVIDERS: &[&dyn ProviderAdapter] = &[&CODEX, &CLAUDE, &OPENCODE_GO, &GROK];

pub(crate) fn adapter(provider_id: &ProviderId) -> anyhow::Result<&'static dyn ProviderAdapter> {
    find(provider_id.as_str()).ok_or_else(|| anyhow::anyhow!("unknown provider: {provider_id}"))
}

pub(crate) fn find(provider_id: &str) -> Option<&'static dyn ProviderAdapter> {
    PROVIDERS
        .iter()
        .copied()
        .find(|provider| provider.manifest().id == provider_id)
}

pub(crate) fn is_supported(provider_id: &str) -> bool {
    find(provider_id).is_some()
}

pub(crate) fn descriptors() -> Vec<ProviderDescriptor> {
    PROVIDERS
        .iter()
        .map(|provider| provider.descriptor())
        .collect()
}

pub(crate) fn execution_policy(provider_id: &ProviderId) -> anyhow::Result<ExecutionPolicy> {
    Ok(adapter(provider_id)?.execution_policy())
}

pub(crate) fn default_provider_configs() -> BTreeMap<String, ProviderConfig> {
    PROVIDERS
        .iter()
        .map(|provider| {
            let manifest = provider.manifest();
            (
                manifest.id.to_string(),
                ProviderConfig {
                    enabled: manifest.default_visible,
                    ..ProviderConfig::default()
                },
            )
        })
        .collect()
}

pub(crate) fn migrate_configs(
    configs: &mut BTreeMap<String, ProviderConfig>,
    discover_local_activity_owners: bool,
) -> anyhow::Result<bool> {
    let mut changed = false;
    for adapter in PROVIDERS {
        if let Some(config) = configs.get_mut(adapter.manifest().id) {
            changed |= adapter.migrate_config(config, discover_local_activity_owners)?;
        }
    }
    Ok(changed)
}

pub(crate) fn remove_unsupported_settings(configs: &mut BTreeMap<String, ProviderConfig>) -> bool {
    let mut changed = false;
    for adapter in PROVIDERS {
        let provider_id = adapter.manifest().id;
        let Some(config) = configs.get_mut(provider_id) else {
            continue;
        };
        let provider_keys = adapter.provider_setting_keys();
        config.settings.retain(|key, _| {
            let supported = provider_keys.contains(&key.as_str());
            if !supported {
                changed = true;
                warn!(
                    provider_id,
                    setting = key,
                    "ignoring unsupported provider setting"
                );
            }
            supported
        });
        let profile_keys = adapter.profile_setting_keys();
        for (index, profile) in config.profiles.iter_mut().enumerate() {
            profile.settings.retain(|key, _| {
                let supported = profile_keys.contains(&key.as_str());
                if !supported {
                    changed = true;
                    warn!(
                        provider_id,
                        profile_index = index,
                        setting = key,
                        "ignoring unsupported provider profile setting"
                    );
                }
                supported
            });
        }
    }
    changed
}

pub(crate) fn build_collectors(config: &Config) -> anyhow::Result<Vec<Arc<dyn ProviderCollector>>> {
    validate_registry(PROVIDERS)?;
    PROVIDERS
        .iter()
        .map(|provider| {
            let id = provider.manifest().id;
            let provider_config = config.providers.get(id).cloned().unwrap_or_default();
            provider
                .validate_config(&provider_config)
                .with_context(|| format!("invalid {id} configuration"))?;
            let collector = provider.build_collector(&provider_config)?;
            anyhow::ensure!(
                collector.provider_id().as_str() == id,
                "provider adapter {id} built a collector for {}",
                collector.provider_id()
            );
            Ok(collector)
        })
        .collect()
}

fn validate_registry(providers: &[&dyn ProviderAdapter]) -> anyhow::Result<()> {
    let mut ids = BTreeMap::new();
    for provider in providers {
        let manifest = provider.manifest();
        anyhow::ensure!(
            !manifest.id.trim().is_empty(),
            "provider id cannot be empty"
        );
        anyhow::ensure!(
            manifest.minimum_refresh_interval_seconds > 0,
            "provider {} has an invalid refresh interval",
            manifest.id
        );
        if ids.insert(manifest.id, ()).is_some() {
            anyhow::bail!("duplicate provider registration: {}", manifest.id);
        }
        let policy = provider.execution_policy();
        anyhow::ensure!(
            policy.max_parallel_accounts > 0,
            "provider {} must allow at least one account collection",
            manifest.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use std::path::PathBuf;

    #[test]
    fn production_registry_is_valid_and_ordered() {
        validate_registry(PROVIDERS).unwrap();
        assert_eq!(
            descriptors()
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            ["codex", "claude", "opencode_go", "grok"]
        );
    }

    #[test]
    fn capabilities_are_derived_from_registered_handlers() {
        for provider in PROVIDERS {
            let descriptor = provider.descriptor();
            assert_eq!(
                descriptor.capabilities.add_account,
                provider.add_account_handler().is_some()
            );
            assert_eq!(
                descriptor.capabilities.repair,
                provider.repair_handler().is_some()
            );
            assert_eq!(
                descriptor.capabilities.launch_account,
                provider.launch_handler().is_some()
            );
            assert_eq!(
                descriptor.capabilities.workspace_setup,
                provider.setup_handler().is_some()
            );
            assert_eq!(
                descriptor.capabilities.setup,
                provider.setup_handler().is_some()
            );
        }
    }

    #[test]
    fn every_adapter_builds_a_matching_collector_with_valid_policy() {
        for provider in PROVIDERS {
            let manifest = provider.manifest();
            provider
                .validate_config(&ProviderConfig::default())
                .unwrap_or_else(|error| panic!("{} default config: {error}", manifest.id));
            let collector = provider
                .build_collector(&ProviderConfig::default())
                .unwrap_or_else(|error| panic!("{} default config: {error}", manifest.id));
            if let Some(watch) = provider
                .local_usage_watch(&ProviderConfig::default())
                .unwrap_or_else(|error| panic!("{} default local watch: {error}", manifest.id))
            {
                watch
                    .validate()
                    .unwrap_or_else(|error| panic!("{} invalid local watch: {error}", manifest.id));
            }
            assert_eq!(collector.provider_id().as_str(), manifest.id);
            assert_eq!(provider.descriptor().id.as_str(), manifest.id);
            let policy = provider.execution_policy();
            assert!(!policy.discovery_timeout.is_zero());
            assert!(!policy.collection_timeout.is_zero());
            assert!(policy.max_parallel_accounts > 0);
        }
    }

    #[test]
    fn every_adapter_rejects_undeclared_provider_and_profile_settings() {
        for provider in PROVIDERS {
            let manifest = provider.manifest();

            let mut provider_config = ProviderConfig::default();
            provider_config
                .settings
                .insert("__undeclared_setting".to_string(), serde_json::json!(true));
            assert!(
                provider.validate_config(&provider_config).is_err(),
                "{} accepted an undeclared provider setting; typed settings structs must use deny_unknown_fields",
                manifest.id
            );

            let mut profile_config = ProviderConfig::default();
            let mut profile = crate::config::ProviderProfileConfig::default();
            profile
                .settings
                .insert("__undeclared_setting".to_string(), serde_json::json!(true));
            profile_config.profiles.push(profile);
            assert!(
                provider.validate_config(&profile_config).is_err(),
                "{} accepted an undeclared profile setting; typed settings structs must use deny_unknown_fields",
                manifest.id
            );
        }
    }

    #[test]
    fn provider_settings_round_trip_without_shared_schema_changes() {
        let value = serde_json::json!({
            "enabled": true,
            "future_provider_option": {"mode": "fast"},
            "profiles": [{"id": "work", "future_profile_option": 42}]
        });
        let config: ProviderConfig = serde_json::from_value(value.clone()).unwrap();
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(
            encoded["future_provider_option"],
            value["future_provider_option"]
        );
        assert_eq!(
            encoded["profiles"][0]["future_profile_option"],
            value["profiles"][0]["future_profile_option"]
        );
    }

    #[test]
    fn typed_settings_can_remove_an_optional_flattened_value() {
        let mut config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "workspace_id": "wrk_old"
        }))
        .unwrap();
        crate::providers::opencode::settings::update_provider(&mut config, |settings| {
            settings.workspace_id = None;
        })
        .unwrap();

        assert!(serde_json::to_value(config)
            .unwrap()
            .get("workspace_id")
            .is_none());
    }

    #[test]
    fn collector_build_rejects_provider_owned_settings_that_are_not_consumed() {
        let mut providers = default_provider_configs();
        providers
            .get_mut("codex")
            .unwrap()
            .settings
            .insert("workspace_id".to_string(), serde_json::json!("wrk_wrong"));
        let config = Config {
            poll_interval_seconds: 300,
            notifications: usage_core::NotificationConfig::default(),
            providers,
            paths: Paths {
                config: PathBuf::from("config.json"),
                db: PathBuf::from("usage.sqlite3"),
                socket: PathBuf::from("usage.sock"),
            },
        };

        let error = match build_collectors(&config) {
            Ok(_) => panic!("unsupported provider settings were accepted"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("invalid codex configuration"));
        assert!(format!("{error:#}").contains("workspace_id"));
    }
}
