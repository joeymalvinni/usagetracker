use serde::{Deserialize, Serialize};

use crate::{config::ProviderConfig, providers::settings_accessors};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenCodeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cookie_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_id: Option<String>,
}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    provider(config)?;
    for (index, profile) in config.profiles.iter().enumerate() {
        profile.ensure_settings_empty(&format!("OpenCode profile at index {index}"))?;
    }
    Ok(())
}

settings_accessors!(provider: OpenCodeSettings);
