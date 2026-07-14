use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{config::ProviderConfig, providers::settings_accessors};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrokSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cookie_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrokProfileSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) grok_home: Option<PathBuf>,
}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    provider(config)?;
    for (index, profile) in config.profiles.iter().enumerate() {
        self::profile(profile)
            .with_context(|| format!("invalid Grok profile configuration at index {index}"))?;
    }
    Ok(())
}

settings_accessors!(provider: GrokSettings);
settings_accessors!(profile: GrokProfileSettings);
