use std::{collections::BTreeMap, sync::Arc};

use usage_core::{ProviderId, ProviderSpec};

use crate::{
    config::Config,
    providers::{
        claude::{ClaudeCollector, PROVIDER_ID as CLAUDE_PROVIDER_ID},
        codex::{CodexCollector, PROVIDER_ID as CODEX_PROVIDER_ID},
        grok::{GrokCollector, PROVIDER_ID as GROK_PROVIDER_ID},
        opencode::{OpenCodeCollector, OPENCODE_GO_PROVIDER_ID},
        ProviderCollector,
    },
    storage::Storage,
};

#[derive(Debug)]
struct ProviderRegistration {
    id: &'static str,
    build: fn(&Config) -> anyhow::Result<Arc<dyn ProviderCollector>>,
}

const PROVIDERS: &[ProviderRegistration] = &[
    ProviderRegistration {
        id: CODEX_PROVIDER_ID,
        build: build_codex,
    },
    ProviderRegistration {
        id: CLAUDE_PROVIDER_ID,
        build: build_claude,
    },
    ProviderRegistration {
        id: OPENCODE_GO_PROVIDER_ID,
        build: build_opencode_go,
    },
    ProviderRegistration {
        id: GROK_PROVIDER_ID,
        build: build_grok,
    },
];

pub(crate) fn ensure_supported(provider_id: &ProviderId) -> anyhow::Result<()> {
    if is_supported(provider_id.as_str()) {
        Ok(())
    } else {
        anyhow::bail!("unknown provider: {provider_id}")
    }
}

pub(crate) fn is_supported(provider_id: &str) -> bool {
    usage_core::provider_spec(provider_id).is_some()
}

pub(crate) async fn build_collectors(
    config: &Config,
    _storage: &Storage,
) -> anyhow::Result<Vec<Arc<dyn ProviderCollector>>> {
    let registrations = validated_registrations(PROVIDERS, usage_core::PROVIDER_SPECS)?;
    let mut providers = Vec::with_capacity(registrations.len());
    for registration in registrations {
        // Provider-level `enabled` is a presentation preference. Collection is
        // paused only at the account level, so hidden providers still need a
        // collector for slow background refreshes and manual refreshes.
        providers.push((registration.build)(config)?);
    }
    Ok(providers)
}

fn validated_registrations<'a>(
    registrations: &'a [ProviderRegistration],
    specs: &[ProviderSpec],
) -> anyhow::Result<Vec<&'a ProviderRegistration>> {
    let mut by_id = BTreeMap::new();
    for registration in registrations {
        if by_id.insert(registration.id, registration).is_some() {
            anyhow::bail!(
                "duplicate provider collector registration: {}",
                registration.id
            );
        }
    }

    let mut ordered = Vec::with_capacity(specs.len());
    for spec in specs {
        let registration = by_id.remove(spec.id).ok_or_else(|| {
            anyhow::anyhow!("missing provider collector registration: {}", spec.id)
        })?;
        ordered.push(registration);
    }
    if let Some(extra_id) = by_id.keys().next() {
        anyhow::bail!("provider collector has no shared specification: {extra_id}");
    }
    Ok(ordered)
}

fn build_codex(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(CodexCollector::new(
        config
            .providers
            .get(CODEX_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
    )?))
}

fn build_claude(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(ClaudeCollector::new(
        config
            .providers
            .get(CLAUDE_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
    )?))
}

fn build_opencode_go(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(OpenCodeCollector::new(
        config
            .providers
            .get(OPENCODE_GO_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
    )?))
}

fn build_grok(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(GrokCollector::new(
        config
            .providers
            .get(GROK_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(id: &'static str) -> ProviderRegistration {
        ProviderRegistration {
            id,
            build: build_codex,
        }
    }

    #[test]
    fn registry_is_validated_and_ordered_by_shared_specs() {
        let registrations = [registration("claude"), registration("codex")];
        let specs = &usage_core::PROVIDER_SPECS[..2];

        let ordered = validated_registrations(&registrations, specs).unwrap();

        assert_eq!(
            ordered.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            ["codex", "claude"]
        );
    }

    #[test]
    fn registry_rejects_missing_duplicate_and_extra_collectors() {
        let specs = &usage_core::PROVIDER_SPECS[..1];

        let missing = validated_registrations(&[], specs).unwrap_err();
        assert!(missing.to_string().contains("missing provider collector"));

        let duplicate = [registration("codex"), registration("codex")];
        let duplicate = validated_registrations(&duplicate, specs).unwrap_err();
        assert!(duplicate
            .to_string()
            .contains("duplicate provider collector"));

        let extra = [registration("codex"), registration("extra")];
        let extra = validated_registrations(&extra, specs).unwrap_err();
        assert!(extra.to_string().contains("no shared specification"));
    }

    #[test]
    fn production_registry_matches_the_shared_provider_catalog() {
        let ordered = validated_registrations(PROVIDERS, usage_core::PROVIDER_SPECS).unwrap();
        assert_eq!(
            ordered.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            usage_core::PROVIDER_SPECS
                .iter()
                .map(|spec| spec.id)
                .collect::<Vec<_>>()
        );
    }
}
