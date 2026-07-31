use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use anyhow::Context;
use usage_core::ImportOptions;

use crate::{config::ProviderConfig, providers::settings_accessors};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaudeLocalImportSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_imported_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) options: Option<ImportOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) manifest: Option<ClaudeImportManifest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaudeImportManifest {
    pub(crate) paths: Vec<String>,
    pub(crate) imported_at: DateTime<Utc>,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_import: Option<ClaudeLocalImportSettings>,
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
        anyhow::ensure!(
            !model.trim_start().starts_with('-'),
            "launch model cannot start with a dash"
        );
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
    use usage_core::{ImportOptions, LaunchFlags};

    #[test]
    fn local_import_settings_round_trip_in_profile() {
        let settings = ClaudeProfileSettings {
            local_import: Some(ClaudeLocalImportSettings {
                last_imported_at: Some(chrono::Utc::now()),
                source: Some("~/.claude".into()),
                source_identity: Some("user@example.com".into()),
                options: Some(ImportOptions::comfort_defaults()),
                manifest: Some(ClaudeImportManifest {
                    paths: vec!["settings.json".into()],
                    imported_at: chrono::Utc::now(),
                }),
            }),
            ..ClaudeProfileSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        let back: ClaudeProfileSettings = serde_json::from_value(value).unwrap();
        assert!(back.local_import.is_some());
    }

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
        for model in [
            "",
            "   ",
            "model'; rm -rf /",
            "--dangerously-skip-permissions",
            long.as_str(),
        ] {
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
