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

impl CodexProfileSettings {
    pub(crate) fn resolved_home(&self, default_home: &std::path::Path) -> PathBuf {
        use crate::providers::paths::expand_home_path;
        self.codex_home
            .as_ref()
            .map(expand_home_path)
            .or_else(|| {
                self.auth_path
                    .as_ref()
                    .map(expand_home_path)
                    .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            })
            .unwrap_or_else(|| default_home.to_path_buf())
    }
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
