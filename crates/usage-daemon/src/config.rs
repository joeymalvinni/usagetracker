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
    default_config_path, default_db_path, default_socket_path, ConfigResponse, NotificationConfig,
    ProviderId, ProviderToggle,
};

use crate::providers::paths::expand_home_path;

const POLL_INTERVAL_ENV: &str = "USAGE_TRACKER_POLL_INTERVAL_SECONDS";
const SUPPORTED_PROVIDER_IDS: [&str; 4] = ["codex", "claude", "opencode_go", "grok"];
pub const MIN_POLL_INTERVAL_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
pub struct Config {
    pub poll_interval_seconds: u64,
    pub notifications: NotificationConfig,
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
    pub notifications: NotificationConfig,
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
    /// Optional provider-specific collection strategy. Providers that do not
    /// expose selectable strategies ignore this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mode: Option<String>,
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
    pub keychain_service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub owns_default_codex_activity: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub owns_default_claude_activity: bool,
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
            keychain_service: None,
            credentials_file: None,
            claude_config_dir: None,
            cli_enabled: None,
            project_roots: Vec::new(),
            owns_default_codex_activity: false,
            owns_default_claude_activity: false,
        }
    }
}

impl Config {
    pub fn load(
        config_override: Option<PathBuf>,
        db_override: Option<PathBuf>,
        socket_override: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        Self::load_inner(config_override, db_override, socket_override, true)
    }

    pub fn load_fixture(
        config_override: Option<PathBuf>,
        db_override: Option<PathBuf>,
        socket_override: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        Self::load_inner(config_override, db_override, socket_override, false)
    }

    fn load_inner(
        config_override: Option<PathBuf>,
        db_override: Option<PathBuf>,
        socket_override: Option<PathBuf>,
        discover_local_activity_owners: bool,
    ) -> anyhow::Result<Self> {
        let paths = default_paths()?;
        let config_path = config_override.unwrap_or(paths.config);
        let db_path = db_override.unwrap_or(paths.db);
        let socket_path = socket_override.unwrap_or(paths.socket);

        let mut file_config = read_or_create_config(&config_path)?;
        add_missing_default_providers(&mut file_config);
        let assigned_codex_owner =
            discover_local_activity_owners && assign_default_codex_activity_owner(&mut file_config);
        let assigned_claude_owner = discover_local_activity_owners
            && assign_default_claude_activity_owner(&mut file_config);
        if assigned_codex_owner || assigned_claude_owner {
            write_config_atomically(&config_path, &file_config)?;
        }
        let poll_interval_seconds = poll_interval_seconds(file_config.poll_interval_seconds)?;
        file_config
            .notifications
            .validate()
            .map_err(|message| anyhow::anyhow!("invalid notification configuration: {message}"))?;

        Ok(Self {
            poll_interval_seconds,
            notifications: file_config.notifications,
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
            notifications: self.notifications.clone(),
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
        notifications: Option<NotificationConfig>,
    ) -> anyhow::Result<()> {
        if let Some(interval) = poll_interval_seconds {
            validate_poll_interval(interval)?;
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
                        source_mode: None,
                    })
                    .enabled = toggle.enabled;
            }
        }
        if let Some(notifications) = notifications {
            notifications
                .validate()
                .map_err(|message| anyhow::anyhow!(message))?;
            self.notifications = notifications;
        }
        Ok(())
    }

    pub fn persist(&self) -> anyhow::Result<()> {
        let file_config = FileConfig {
            poll_interval_seconds: self.poll_interval_seconds,
            notifications: self.notifications.clone(),
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

fn assign_default_codex_activity_owner(config: &mut FileConfig) -> bool {
    let Some(local_codex_home) = default_codex_home() else {
        return false;
    };
    assign_default_codex_activity_owner_for_home(config, &local_codex_home)
}

fn assign_default_codex_activity_owner_for_home(
    config: &mut FileConfig,
    local_codex_home: &Path,
) -> bool {
    let Some(local_account_id) = codex_account_id_from_auth(&local_codex_home.join("auth.json"))
    else {
        return false;
    };
    let Some(provider) = config.providers.get_mut("codex") else {
        return false;
    };
    if provider
        .profiles
        .iter()
        .any(|profile| profile.enabled && !profile.deleted && profile.owns_default_codex_activity)
    {
        return false;
    }

    let active_profile_homes = provider
        .profiles
        .iter()
        .filter(|profile| profile.enabled && !profile.deleted)
        .map(|profile| {
            profile
                .codex_home
                .as_deref()
                .map(expand_home_path)
                .unwrap_or_else(|| local_codex_home.to_path_buf())
        })
        .collect::<Vec<_>>();
    if active_profile_homes
        .iter()
        .any(|codex_home| codex_home == local_codex_home)
    {
        return false;
    }

    let matching = provider
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .filter_map(|(index, profile)| {
            let codex_home = profile
                .codex_home
                .as_deref()
                .map(expand_home_path)
                .unwrap_or_else(|| local_codex_home.to_path_buf());
            if codex_home == local_codex_home {
                return None;
            }
            let auth_path = profile
                .auth_path
                .as_deref()
                .map(expand_home_path)
                .unwrap_or_else(|| codex_home.join("auth.json"));
            (codex_account_id_from_auth(&auth_path).as_deref() == Some(local_account_id.as_str()))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return false;
    }
    provider.profiles[matching[0]].owns_default_codex_activity = true;
    true
}

fn default_codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| expand_home_path(&path))
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}

fn codex_account_id_from_auth(path: &Path) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    value
        .get("tokens")?
        .get("account_id")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn assign_default_claude_activity_owner(config: &mut FileConfig) -> bool {
    let Some(provider) = config.providers.get_mut("claude") else {
        return false;
    };
    let active = provider
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if active.len() != 1 {
        return false;
    }
    let profile = &provider.profiles[active[0]];
    if profile.owns_default_claude_activity
        || profile.claude_config_dir.is_none()
        || profile
            .project_roots
            .iter()
            .any(|root| is_default_claude_project_root(root))
    {
        return false;
    }
    provider.profiles[active[0]].owns_default_claude_activity = true;
    true
}

fn is_default_claude_project_root(path: &Path) -> bool {
    let value = path.to_string_lossy();
    value.ends_with("/.claude/projects") || value.ends_with("/.config/claude/projects")
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

    validate_poll_interval(value)?;
    Ok(value)
}

fn validate_poll_interval(value: u64) -> anyhow::Result<()> {
    if value < MIN_POLL_INTERVAL_SECONDS {
        bail!("poll interval must be at least {MIN_POLL_INTERVAL_SECONDS} seconds");
    }
    Ok(())
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
                source_mode: None,
            },
        );
        providers.insert(
            "claude".to_string(),
            ProviderConfig {
                enabled: false,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
                source_mode: None,
            },
        );
        providers.insert(
            "opencode_go".to_string(),
            ProviderConfig {
                enabled: false,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
                source_mode: None,
            },
        );
        providers.insert(
            "grok".to_string(),
            ProviderConfig {
                enabled: false,
                profiles: Vec::new(),
                cookie_header: None,
                workspace_id: None,
                source_mode: None,
            },
        );
        Self {
            poll_interval_seconds: default_poll_interval_seconds(),
            notifications: NotificationConfig::default(),
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
        assert!(!config.providers["grok"].enabled);
        assert!(config.notifications.enabled);
    }

    #[test]
    fn older_config_defaults_notifications_to_enabled() {
        let config: FileConfig =
            serde_json::from_str(r#"{"poll_interval_seconds":300,"providers":{}}"#).unwrap();
        assert!(config.notifications.enabled);
    }

    #[test]
    fn preserves_provider_source_mode_without_emitting_empty_defaults() {
        let config: FileConfig =
            serde_json::from_str(r#"{"providers":{"grok":{"enabled":true,"source_mode":"web"}}}"#)
                .unwrap();
        assert_eq!(config.providers["grok"].source_mode.as_deref(), Some("web"));
        let defaults = serde_json::to_value(FileConfig::default()).unwrap();
        assert!(defaults["providers"]["grok"].get("source_mode").is_none());
    }

    #[test]
    fn rejects_zero_poll_interval() {
        let err = poll_interval_seconds(0).unwrap_err();
        assert!(err.to_string().contains("at least 60 seconds"));
    }

    #[test]
    fn rejects_unsafe_nonzero_poll_interval() {
        let err = validate_poll_interval(59).unwrap_err();
        assert!(err.to_string().contains("at least 60 seconds"));
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

        assert_eq!(config.providers.len(), 4);
        assert!(!config.providers["codex"].enabled);
        assert!(config.providers.contains_key("claude"));
        assert!(config.providers.contains_key("opencode_go"));
        assert!(config.providers.contains_key("grok"));
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

    #[test]
    fn sole_managed_claude_profile_adopts_unowned_default_activity() {
        let mut config = FileConfig::default();
        config.providers.get_mut("claude").unwrap().profiles = vec![ProviderProfileConfig {
            id: Some("managed".to_string()),
            claude_config_dir: Some(PathBuf::from("/profiles/managed")),
            project_roots: vec![PathBuf::from("/profiles/managed/projects")],
            ..ProviderProfileConfig::default()
        }];

        assert!(assign_default_claude_activity_owner(&mut config));
        assert!(config.providers["claude"].profiles[0].owns_default_claude_activity);
        assert!(!assign_default_claude_activity_owner(&mut config));
    }

    #[test]
    fn matching_codex_profile_durably_adopts_default_activity() {
        let root = std::env::temp_dir().join(format!("codex-owner-{}", uuid::Uuid::new_v4()));
        let local_home = root.join("default");
        let personal_home = root.join("personal");
        let work_home = root.join("work");
        fs::create_dir_all(&local_home).unwrap();
        fs::create_dir_all(&personal_home).unwrap();
        fs::create_dir_all(&work_home).unwrap();
        fs::write(
            local_home.join("auth.json"),
            r#"{"tokens":{"account_id":"personal"}}"#,
        )
        .unwrap();
        fs::write(
            personal_home.join("auth.json"),
            r#"{"tokens":{"account_id":"personal"}}"#,
        )
        .unwrap();
        fs::write(
            work_home.join("auth.json"),
            r#"{"tokens":{"account_id":"work"}}"#,
        )
        .unwrap();

        let mut config = FileConfig::default();
        config.providers.get_mut("codex").unwrap().profiles = vec![
            ProviderProfileConfig {
                id: Some("personal".to_string()),
                codex_home: Some(personal_home),
                ..ProviderProfileConfig::default()
            },
            ProviderProfileConfig {
                id: Some("work".to_string()),
                codex_home: Some(work_home),
                ..ProviderProfileConfig::default()
            },
        ];

        assert!(assign_default_codex_activity_owner_for_home(
            &mut config,
            &local_home
        ));
        assert!(config.providers["codex"].profiles[0].owns_default_codex_activity);
        assert!(!config.providers["codex"].profiles[1].owns_default_codex_activity);

        let persisted: FileConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert!(persisted.providers["codex"].profiles[0].owns_default_codex_activity);
        assert!(!persisted.providers["codex"].profiles[1].owns_default_codex_activity);

        fs::write(
            local_home.join("auth.json"),
            r#"{"tokens":{"account_id":"work"}}"#,
        )
        .unwrap();
        assert!(!assign_default_codex_activity_owner_for_home(
            &mut config,
            &local_home
        ));
        assert!(config.providers["codex"].profiles[0].owns_default_codex_activity);
        assert!(!config.providers["codex"].profiles[1].owns_default_codex_activity);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_default_codex_profile_prevents_managed_duplicate_owner() {
        let root = std::env::temp_dir().join(format!("codex-owner-{}", uuid::Uuid::new_v4()));
        let local_home = root.join("default");
        let managed_home = root.join("managed");
        fs::create_dir_all(&local_home).unwrap();
        fs::create_dir_all(&managed_home).unwrap();
        for home in [&local_home, &managed_home] {
            fs::write(
                home.join("auth.json"),
                r#"{"tokens":{"account_id":"same-account"}}"#,
            )
            .unwrap();
        }

        let mut config = FileConfig::default();
        config.providers.get_mut("codex").unwrap().profiles = vec![
            ProviderProfileConfig {
                id: Some("default".to_string()),
                codex_home: Some(local_home.clone()),
                ..ProviderProfileConfig::default()
            },
            ProviderProfileConfig {
                id: Some("managed".to_string()),
                codex_home: Some(managed_home),
                ..ProviderProfileConfig::default()
            },
        ];

        assert!(!assign_default_codex_activity_owner_for_home(
            &mut config,
            &local_home
        ));
        assert!(config.providers["codex"]
            .profiles
            .iter()
            .all(|profile| !profile.owns_default_codex_activity));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multiple_managed_claude_profiles_never_share_default_activity() {
        let mut config = FileConfig::default();
        config.providers.get_mut("claude").unwrap().profiles = vec![
            ProviderProfileConfig {
                id: Some("personal".to_string()),
                claude_config_dir: Some(PathBuf::from("/profiles/personal")),
                ..ProviderProfileConfig::default()
            },
            ProviderProfileConfig {
                id: Some("work".to_string()),
                claude_config_dir: Some(PathBuf::from("/profiles/work")),
                ..ProviderProfileConfig::default()
            },
        ];

        assert!(!assign_default_claude_activity_owner(&mut config));
        assert!(config.providers["claude"]
            .profiles
            .iter()
            .all(|profile| !profile.owns_default_claude_activity));
    }

    #[test]
    fn loading_config_persists_default_claude_activity_owner_migration() {
        let root = std::env::temp_dir().join(format!("usage-config-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let mut file_config = FileConfig::default();
        file_config.providers.get_mut("claude").unwrap().profiles = vec![ProviderProfileConfig {
            id: Some("managed".to_string()),
            claude_config_dir: Some(root.join("profiles/managed")),
            project_roots: vec![root.join("profiles/managed/projects")],
            ..ProviderProfileConfig::default()
        }];
        write_config_atomically(&config_path, &file_config).unwrap();

        let loaded = Config::load(
            Some(config_path.clone()),
            Some(root.join("usage.sqlite3")),
            Some(root.join("usage.sock")),
        )
        .unwrap();
        let persisted: FileConfig =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert!(loaded.providers["claude"].profiles[0].owns_default_claude_activity);
        assert!(persisted.providers["claude"].profiles[0].owns_default_claude_activity);
        fs::remove_dir_all(root).unwrap();
    }
}
