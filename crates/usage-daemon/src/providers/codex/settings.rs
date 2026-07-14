use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use anyhow::Context;

use crate::{config::ProviderConfig, providers::settings_accessors};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexProfileSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) codex_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "crate::config::is_false")]
    pub(crate) owns_default_codex_activity: bool,
}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    config.ensure_settings_empty("Codex provider")?;
    for (index, profile) in config.profiles.iter().enumerate() {
        self::profile(profile)
            .with_context(|| format!("invalid Codex profile configuration at index {index}"))?;
    }
    Ok(())
}

settings_accessors!(profile: CodexProfileSettings);
