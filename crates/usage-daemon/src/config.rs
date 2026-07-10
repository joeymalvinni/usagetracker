use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use usage_core::{
    default_config_path, default_db_path, default_socket_path, ConfigResponse, ProviderId,
    ProviderToggle,
};

const POLL_INTERVAL_ENV: &str = "USAGE_TRACKER_POLL_INTERVAL_SECONDS";
const SUPPORTED_PROVIDER_IDS: [&str; 3] = ["codex", "claude", "opencode_go"];

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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProviderProfileConfig>,
    #[serde(default)]
    pub cookie_header: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default = "default_profile_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keychain_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_roots: Vec<PathBuf>,
}

impl Default for ProviderProfileConfig {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            deleted: false,
            display_name: None,
            auth_path: None,
            codex_home: None,
            keychain_account: None,
            credentials_file: None,
            cli_enabled: None,
            project_roots: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(
        config_override: Option<PathBuf>,
        db_override: Option<PathBuf>,
        socket_override: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let paths = default_paths()?;
        let config_path = config_override.unwrap_or(paths.config);
        let db_path = db_override.unwrap_or(paths.db);
        let socket_path = socket_override.unwrap_or(paths.socket);

        let mut file_config = read_or_create_config(&config_path)?;
        add_missing_default_providers(&mut file_config);
        let poll_interval_seconds = poll_interval_seconds(file_config.poll_interval_seconds)?;

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
            .filter(|(id, _)| is_supported_provider(id))
            .filter(|(_, config)| config.enabled)
            .map(|(id, _)| ProviderId::new(id.clone()))
            .collect()
    }

    pub fn response_with_visible_providers(
        &self,
        visible_providers: Option<&BTreeSet<String>>,
    ) -> ConfigResponse {
        ConfigResponse {
            poll_interval_seconds: self.poll_interval_seconds,
            config_path: self.paths.config.display().to_string(),
            socket_path: self.paths.socket.display().to_string(),
            db_path: self.paths.db.display().to_string(),
            enabled_providers: self
                .enabled_provider_ids()
                .into_iter()
                .filter(|id| provider_visible(id.as_str(), visible_providers))
                .collect(),
            providers: self
                .providers
                .iter()
                .filter(|(id, _)| is_supported_provider(id))
                .filter(|(id, _)| provider_visible(id, visible_providers))
                .map(|(id, provider)| {
                    (
                        id.clone(),
                        ProviderToggle {
                            enabled: provider.enabled,
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn apply_update(
        &mut self,
        poll_interval_seconds: Option<u64>,
        providers: Option<&BTreeMap<String, ProviderToggle>>,
    ) -> anyhow::Result<()> {
        if let Some(interval) = poll_interval_seconds {
            if interval == 0 {
                bail!("poll interval must be greater than zero");
            }
            self.poll_interval_seconds = interval;
        }
        if let Some(providers) = providers {
            for (id, toggle) in providers {
                self.providers
                    .entry(id.clone())
                    .or_insert(ProviderConfig {
                        enabled: false,
                        profiles: Vec::new(),
                        cookie_header: None,
                        workspace_id: None,
                    })
                    .enabled = toggle.enabled;
            }
        }
        Ok(())
    }

    pub fn persist(&self) -> anyhow::Result<()> {
        let file_config = FileConfig {
            poll_interval_seconds: self.poll_interval_seconds,
            providers: self.providers.clone(),
            debug_capture_raw_payloads: self.debug_capture_raw_payloads,
        };
        write_config_atomically(&self.paths.config, &file_config)
    }
}

fn provider_visible(provider_id: &str, visible_providers: Option<&BTreeSet<String>>) -> bool {
    visible_providers.is_none_or(|ids| ids.contains(provider_id))
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_supported_provider(provider_id: &str) -> bool {
    SUPPORTED_PROVIDER_IDS.contains(&provider_id)
}

fn add_missing_default_providers(config: &mut FileConfig) {
    for (id, provider) in FileConfig::default().providers {
        config.providers.entry(id).or_insert(provider);
    }
}

fn default_paths() -> anyhow::Result<Paths> {
    Ok(Paths {
        config: default_config_path().context("failed to resolve ~/.usagetracker/config.json")?,
        db: default_db_path().context("failed to resolve ~/.usagetracker/usage.sqlite3")?,
        socket: default_socket_path().context("failed to resolve ~/.usagetracker/usage.sock")?,
    })
}

fn poll_interval_seconds(file_value: u64) -> anyhow::Result<u64> {
    let value = match std::env::var(POLL_INTERVAL_ENV) {
        Ok(value) => parse_poll_interval_env(&value)?,
        Err(std::env::VarError::NotPresent) => file_value,
        Err(err) => bail!("failed to read {POLL_INTERVAL_ENV}: {err}"),
    };

    if value == 0 {
        bail!("poll interval must be greater than zero");
    }
    Ok(value)
}

fn parse_poll_interval_env(value: &str) -> anyhow::Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("{POLL_INTERVAL_ENV} must be an integer number of seconds"))
}

impl Default for FileConfig {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            ProviderConfig {
                enabled: true,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
            },
        );
        providers.insert(
            "claude".to_string(),
            ProviderConfig {
                enabled: false,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
            },
        );
        providers.insert(
            "opencode_go".to_string(),
            ProviderConfig {
                enabled: false,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
            },
        );
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
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)?;
        return Ok(serde_json::from_str(&contents)?);
    }

    let config = FileConfig::default();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    write_config_atomically(path, &config)?;
    Ok(config)
}

fn write_config_atomically(path: &Path, config: &FileConfig) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(config)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        if let Ok(directory) = fs::File::open(parent) {
            directory.sync_all()?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.with_context(|| format!("failed to write {}", path.display()))
}

fn default_poll_interval_seconds() -> u64 {
    300
}

fn default_profile_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn default_config_enables_codex_only() {
        let config = FileConfig::default();
        assert!(config.providers["codex"].enabled);
        assert!(!config.providers["claude"].enabled);
    }

    #[test]
    fn rejects_zero_poll_interval() {
        let err = poll_interval_seconds(0).unwrap_err();
        assert!(err.to_string().contains("greater than zero"));
    }

    #[test]
    fn rejects_malformed_poll_interval_env_value() {
        let err = parse_poll_interval_env("soon").unwrap_err();
        assert!(err.to_string().contains("must be an integer"));
    }

    #[test]
    fn defaults_live_under_usagetracker_home_dir() {
        let paths = default_paths().unwrap();
        assert!(paths.config.ends_with(".usagetracker/config.json"));
        assert!(paths.db.ends_with(".usagetracker/usage.sqlite3"));
        assert!(paths.socket.ends_with(".usagetracker/usage.sock"));
    }

    #[test]
    fn atomically_written_configs_are_private() {
        let root = std::env::temp_dir().join(format!("usage-config-{}", uuid::Uuid::new_v4()));
        let path = root.join("config.json");

        write_config_atomically(&path, &FileConfig::default()).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fills_provider_defaults_for_older_configs() {
        let mut config = FileConfig {
            providers: BTreeMap::from([(
                "codex".to_string(),
                ProviderConfig {
                    enabled: false,
                    ..ProviderConfig::default()
                },
            )]),
            ..FileConfig::default()
        };

        add_missing_default_providers(&mut config);

        assert_eq!(config.providers.len(), 3);
        assert!(!config.providers["codex"].enabled);
        assert!(config.providers.contains_key("claude"));
        assert!(config.providers.contains_key("opencode_go"));
        assert!(!is_supported_provider("unknown"));
        assert!(!is_supported_provider("opencode"));
    }

    #[test]
    fn tightens_permissions_on_existing_configs() {
        let root = std::env::temp_dir().join(format!("usage-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        fs::write(&path, serde_json::to_vec(&FileConfig::default()).unwrap()).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();

        read_or_create_config(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
}
