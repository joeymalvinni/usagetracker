use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorSettings {}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    provider(config)?;
    for (index, profile) in config.profiles.iter().enumerate() {
        profile.ensure_settings_empty(&format!("Cursor profile at index {index}"))?;
    }
    Ok(())
}

pub(crate) fn provider(config: &ProviderConfig) -> anyhow::Result<CursorSettings> {
    config.settings()
}
