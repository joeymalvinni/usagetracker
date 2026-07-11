use std::sync::Arc;

use usage_core::ProviderId;

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
    PROVIDERS
        .iter()
        .any(|registration| registration.id == provider_id)
}

pub(crate) async fn build_enabled(
    config: &Config,
    _storage: &Storage,
) -> anyhow::Result<Vec<Arc<dyn ProviderCollector>>> {
    let mut providers = Vec::new();
    for registration in PROVIDERS {
        // Provider-level `enabled` is a presentation preference. Collection is
        // paused only at the account level, so hidden providers still need a
        // collector for slow background refreshes and manual refreshes.
        providers.push((registration.build)(config)?);
    }
    Ok(providers)
}

fn build_codex(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(CodexCollector::new(
        config
            .providers
            .get(CODEX_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}

fn build_claude(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(ClaudeCollector::new(
        config
            .providers
            .get(CLAUDE_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}

fn build_opencode_go(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(OpenCodeCollector::new(
        config
            .providers
            .get(OPENCODE_GO_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}

fn build_grok(config: &Config) -> anyhow::Result<Arc<dyn ProviderCollector>> {
    Ok(Arc::new(GrokCollector::new(
        config
            .providers
            .get(GROK_PROVIDER_ID)
            .cloned()
            .unwrap_or_default(),
        config.debug_capture_raw_payloads,
    )?))
}
