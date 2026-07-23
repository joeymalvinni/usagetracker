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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) launch: Option<usage_core::LaunchFlags>,
}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    config.ensure_settings_empty("Claude provider")?;
    for (index, profile) in config.profiles.iter().enumerate() {
        let settings = self::profile(profile)
            .with_context(|| format!("invalid Claude profile configuration at index {index}"))?;
        if let Some(flags) = &settings.launch {
            validate_launch_flags(flags)
                .with_context(|| format!("invalid Claude launch flags at index {index}"))?;
        }
    }
    Ok(())
}

/// Effort is a closed enum at the wire layer; the model string is constrained
/// here because both end up inside a generated shell script.
pub(crate) fn validate_launch_flags(flags: &usage_core::LaunchFlags) -> anyhow::Result<()> {
    if let Some(model) = &flags.model {
        anyhow::ensure!(!model.trim().is_empty(), "launch model cannot be blank");
        anyhow::ensure!(model.len() <= 128, "launch model is too long");
        anyhow::ensure!(
            model
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "._:/-@".contains(ch)),
            "launch model contains unsupported characters"
        );
    }
    Ok(())
}

settings_accessors!(profile: ClaudeProfileSettings);

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::LaunchFlags;

    #[test]
    fn launch_flag_validation_constrains_the_model_string() {
        let valid = LaunchFlags {
            model: Some("claude-fable-5".to_string()),
            ..LaunchFlags::default()
        };
        assert!(validate_launch_flags(&valid).is_ok());

        for model in [
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "claude-sonnet-4@20250514",
        ] {
            let flags = LaunchFlags {
                model: Some(model.to_string()),
                ..LaunchFlags::default()
            };
            assert!(
                validate_launch_flags(&flags).is_ok(),
                "model {model:?} should be accepted"
            );
        }

        let long = "a".repeat(129);
        for model in ["", "   ", "model'; rm -rf /", long.as_str()] {
            let flags = LaunchFlags {
                model: Some(model.to_string()),
                ..LaunchFlags::default()
            };
            assert!(
                validate_launch_flags(&flags).is_err(),
                "model {model:?} should be rejected"
            );
        }
    }
}
