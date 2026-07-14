use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use anyhow::Context;

use crate::{config::ProviderConfig, providers::settings_accessors};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaudeProfileSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) keychain_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) keychain_service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) credentials_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) claude_config_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cli_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) project_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "crate::config::is_false")]
    pub(crate) owns_default_claude_activity: bool,
}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    config.ensure_settings_empty("Claude provider")?;
    for (index, profile) in config.profiles.iter().enumerate() {
        self::profile(profile)
            .with_context(|| format!("invalid Claude profile configuration at index {index}"))?;
    }
    Ok(())
}

settings_accessors!(profile: ClaudeProfileSettings);
