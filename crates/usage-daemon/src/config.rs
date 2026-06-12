use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use usage_core::{ConfigResponse, ProviderId};

#[derive(Clone, Debug)]
pub struct Config {
    pub poll_interval_seconds: u64,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub debug_capture_raw_payloads: bool,
    pub paths: Paths,
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub db: PathBuf,
    pub socket: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default = "default_poll_interval_seconds")]
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub debug_capture_raw_payloads: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Config {
    pub fn load(
        config_override: Option<PathBuf>,
        db_override: Option<PathBuf>,
        socket_override: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let config_path = config_override.unwrap_or_else(|| PathBuf::from("./config.json"));
        let db_path = db_override.unwrap_or_else(|| PathBuf::from("./usage.sqlite3"));
        let socket_path = socket_override.unwrap_or_else(|| PathBuf::from("./usage.sock"));

        let file_config = read_or_create_config(&config_path)?;
        let poll_interval_seconds = std::env::var("USAGE_TRACKER_POLL_INTERVAL_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(file_config.poll_interval_seconds);

        Ok(Self {
            poll_interval_seconds,
            providers: file_config.providers,
            debug_capture_raw_payloads: file_config.debug_capture_raw_payloads,
            paths: Paths {
                config: config_path,
                db: db_path,
                socket: socket_path,
            },
        })
    }

    pub fn provider_enabled(&self, provider: &str) -> bool {
        self.providers
            .get(provider)
            .map(|provider| provider.enabled)
            .unwrap_or(false)
    }

    pub fn enabled_provider_ids(&self) -> Vec<ProviderId> {
        self.providers
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(id, _)| ProviderId::new(id.clone()))
            .collect()
    }

    pub fn response(&self) -> ConfigResponse {
        ConfigResponse {
            poll_interval_seconds: self.poll_interval_seconds,
            config_path: self.paths.config.display().to_string(),
            socket_path: self.paths.socket.display().to_string(),
            db_path: self.paths.db.display().to_string(),
            enabled_providers: self.enabled_provider_ids(),
        }
    }
}

impl Default for FileConfig {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert("codex".to_string(), ProviderConfig { enabled: true });
        providers.insert("claude".to_string(), ProviderConfig { enabled: false });
        Self {
            poll_interval_seconds: default_poll_interval_seconds(),
            providers,
            debug_capture_raw_payloads: false,
        }
    }
}

fn read_or_create_config(path: &Path) -> anyhow::Result<FileConfig> {
    if path.exists() {
        let contents = fs::read_to_string(path)?;
        return Ok(serde_json::from_str(&contents)?);
    }

    let config = FileConfig::default();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(&config)?)?;
    Ok(config)
}

fn default_poll_interval_seconds() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_codex_only() {
        let config = FileConfig::default();
        assert!(config.providers["codex"].enabled);
        assert!(!config.providers["claude"].enabled);
    }
}
