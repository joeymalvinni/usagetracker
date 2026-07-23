use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use usage_core::{
    default_config_path, default_db_path, default_socket_path, ConfigResponse, NotificationConfig,
    ProviderId, ProviderToggle,
};

const POLL_INTERVAL_ENV: &str = "USAGE_TRACKER_POLL_INTERVAL_SECONDS";
pub const MIN_POLL_INTERVAL_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
pub struct Config {
    pub poll_interval_seconds: u64,
    pub notifications: NotificationConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
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
    /// Accepted only to migrate older config files; raw payload capture was removed.
    #[serde(default, rename = "debug_capture_raw_payloads", skip_serializing)]
    _legacy_debug_capture_raw_payloads: bool,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProviderProfileConfig>,
    /// Provider-owned configuration. Flattening preserves the existing JSON
    /// format while keeping the shared envelope independent of every provider.
    #[serde(default, flatten)]
    pub(crate) settings: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default = "default_profile_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Provider-owned profile configuration, serialized alongside the common
    /// profile fields for backward compatibility.
    #[serde(default, flatten)]
    pub(crate) settings: BTreeMap<String, serde_json::Value>,
}

impl Default for ProviderProfileConfig {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            deleted: false,
            display_name: None,
            settings: BTreeMap::new(),
        }
    }
}

impl ProviderConfig {
    pub(crate) fn settings<T: DeserializeOwned + Default>(&self) -> anyhow::Result<T> {
        settings_from_map(&self.settings)
    }

    pub(crate) fn patch_settings<T: Serialize>(&mut self, settings: &T) -> anyhow::Result<()> {
        patch_settings_map(&mut self.settings, settings)
    }

    pub(crate) fn ensure_settings_empty(&self, owner: &str) -> anyhow::Result<()> {
        ensure_settings_empty(&self.settings, owner)
    }
}

impl ProviderProfileConfig {
    pub(crate) fn settings<T: DeserializeOwned + Default>(&self) -> anyhow::Result<T> {
        settings_from_map(&self.settings)
    }

    pub(crate) fn patch_settings<T: Serialize>(&mut self, settings: &T) -> anyhow::Result<()> {
        patch_settings_map(&mut self.settings, settings)
    }

    pub(crate) fn clear_settings(&mut self) {
        self.settings.clear();
    }

    pub(crate) fn ensure_settings_empty(&self, owner: &str) -> anyhow::Result<()> {
        ensure_settings_empty(&self.settings, owner)
    }
}

fn ensure_settings_empty(
    settings: &BTreeMap<String, serde_json::Value>,
    owner: &str,
) -> anyhow::Result<()> {
    let keys = settings
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    if keys.is_empty() {
        Ok(())
    } else {
        bail!("{owner} does not support settings: {}", keys.join(", "))
    }
}

fn settings_from_map<T: DeserializeOwned + Default>(
    settings: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<T> {
    // Older releases serialized shared optional provider fields as `null` on
    // every provider. Null carries no setting value, so ignore it during typed
    // decoding as a second line of defense behind the load-time migration.
    let values = settings
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    if values.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_value(serde_json::Value::Object(values))
        .context("invalid provider-owned configuration")
}

fn patch_settings_map<T: Serialize>(
    destination: &mut BTreeMap<String, serde_json::Value>,
    settings: &T,
) -> anyhow::Result<()> {
    let serde_json::Value::Object(values) = serde_json::to_value(settings)? else {
        bail!("provider-owned configuration must serialize as an object");
    };
    destination.clear();
    for (key, value) in values {
        if value.is_null() || value.as_array().is_some_and(Vec::is_empty) {
            destination.remove(&key);
        } else {
            destination.insert(key, value);
        }
    }
    Ok(())
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
        let uses_default_app_directory =
            config_override.is_none() || db_override.is_none() || socket_override.is_none();
        let paths = default_paths()?;
        if uses_default_app_directory {
            let app_directory = paths
                .config
                .parent()
                .ok_or_else(|| anyhow::anyhow!("default config path has no parent directory"))?;
            ensure_private_directory(app_directory)?;
        }
        let config_path = config_override.unwrap_or(paths.config);
        let db_path = db_override.unwrap_or(paths.db);
        let socket_path = socket_override.unwrap_or(paths.socket);

        let mut file_config = read_or_create_config(&config_path)?;
        add_missing_default_providers(&mut file_config);
        let removed_legacy_nulls = remove_null_provider_settings(&mut file_config.providers);
        let removed_unsupported_settings =
            crate::runtime::provider_registry::remove_unsupported_settings(
                &mut file_config.providers,
            );
        let migrated_provider_config = crate::runtime::provider_registry::migrate_configs(
            &mut file_config.providers,
            discover_local_activity_owners,
        )?;
        if removed_legacy_nulls || removed_unsupported_settings || migrated_provider_config {
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
                self.providers.entry(id.clone()).or_default().enabled = toggle.enabled;
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
            _legacy_debug_capture_raw_payloads: false,
            notifications: self.notifications.clone(),
            providers: self.providers.clone(),
        };
        write_config_atomically(&self.paths.config, &file_config)
    }
}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect private directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "private directory path {} is not a directory",
            path.display()
        );
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to secure private directory {}", path.display()))
}

fn provider_visible(provider_id: &str, visible_providers: Option<&BTreeSet<String>>) -> bool {
    visible_providers.is_none_or(|ids| ids.contains(provider_id))
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

fn is_supported_provider(provider_id: &str) -> bool {
    crate::runtime::provider_registry::is_supported(provider_id)
}

fn add_missing_default_providers(config: &mut FileConfig) {
    for (id, provider) in FileConfig::default().providers {
        config.providers.entry(id).or_insert(provider);
    }
}

/// Removes valueless flattened settings emitted by the pre-adapter shared
/// provider schema. This is deliberately value-based rather than key-based so
/// future adapters remain free to own new keys without changing this loader.
fn remove_null_provider_settings(providers: &mut BTreeMap<String, ProviderConfig>) -> bool {
    let mut changed = false;
    for provider in providers.values_mut() {
        let before = provider.settings.len();
        provider.settings.retain(|_, value| !value.is_null());
        changed |= provider.settings.len() != before;
        for profile in &mut provider.profiles {
            let before = profile.settings.len();
            profile.settings.retain(|_, value| !value.is_null());
            changed |= profile.settings.len() != before;
        }
    }
    changed
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
        let providers = crate::runtime::provider_registry::default_provider_configs();
        Self {
            poll_interval_seconds: default_poll_interval_seconds(),
            _legacy_debug_capture_raw_payloads: false,
            notifications: NotificationConfig::default(),
            providers,
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

    fn codex_profile(id: &str, home: PathBuf) -> ProviderProfileConfig {
        let mut profile = ProviderProfileConfig {
            id: Some(id.to_string()),
            ..ProviderProfileConfig::default()
        };
        crate::providers::codex::settings::update_profile(&mut profile, |settings| {
            settings.codex_home = Some(home);
        })
        .unwrap();
        profile
    }

    fn claude_profile(
        id: &str,
        config_dir: PathBuf,
        project_roots: Vec<PathBuf>,
    ) -> ProviderProfileConfig {
        let mut profile = ProviderProfileConfig {
            id: Some(id.to_string()),
            ..ProviderProfileConfig::default()
        };
        crate::providers::claude::settings::update_profile(&mut profile, |settings| {
            settings.claude_config_dir = Some(config_dir);
            settings.project_roots = project_roots;
        })
        .unwrap();
        profile
    }

    #[test]
    fn default_config_enables_codex_only() {
        let config = FileConfig::default();
        assert!(config.providers["codex"].enabled);
        assert!(!config.providers["claude"].enabled);
        assert!(!config.providers["cursor"].enabled);
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
    fn accepts_but_drops_removed_raw_payload_setting() {
        let config: FileConfig =
            serde_json::from_str(r#"{"debug_capture_raw_payloads":true,"providers":{}}"#).unwrap();
        let encoded = serde_json::to_value(config).unwrap();
        assert!(encoded.get("debug_capture_raw_payloads").is_none());
    }

    #[test]
    fn preserves_provider_source_mode_without_emitting_empty_defaults() {
        let config: FileConfig =
            serde_json::from_str(r#"{"providers":{"grok":{"enabled":true,"source_mode":"web"}}}"#)
                .unwrap();
        assert_eq!(
            crate::providers::grok::settings::provider(&config.providers["grok"])
                .unwrap()
                .source_mode
                .as_deref(),
            Some("web")
        );
        let defaults = serde_json::to_value(FileConfig::default()).unwrap();
        assert!(defaults["providers"]["grok"].get("source_mode").is_none());
    }

    #[test]
    fn round_trips_isolated_grok_profile_homes() {
        let config: FileConfig = serde_json::from_str(
            r#"{"providers":{"grok":{"enabled":true,"profiles":[{"id":"work","grok_home":"~/.usagetracker/profiles/grok/work"}]}}}"#,
        )
        .unwrap();

        let profile = &config.providers["grok"].profiles[0];
        assert_eq!(profile.id.as_deref(), Some("work"));
        assert_eq!(
            crate::providers::grok::settings::profile(profile)
                .unwrap()
                .grok_home
                .as_deref(),
            Some(Path::new("~/.usagetracker/profiles/grok/work"))
        );
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(
            encoded["providers"]["grok"]["profiles"][0]["grok_home"],
            "~/.usagetracker/profiles/grok/work"
        );
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

        assert_eq!(config.providers.len(), 5);
        assert!(!config.providers["codex"].enabled);
        assert!(config.providers.contains_key("claude"));
        assert!(config.providers.contains_key("cursor"));
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
    fn creates_and_tightens_private_directory_permissions() {
        let root = std::env::temp_dir().join(format!("usage-private-{}", uuid::Uuid::new_v4()));
        let path = root.join(".usagetracker");
        fs::create_dir_all(&path).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();

        ensure_private_directory(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_a_symlink_for_the_private_directory() {
        let root = std::env::temp_dir().join(format!("usage-private-{}", uuid::Uuid::new_v4()));
        let target = root.join("target");
        let path = root.join(".usagetracker");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let error = ensure_private_directory(&path).unwrap_err();

        assert!(error.to_string().contains("is not a directory"));
        assert!(target.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sole_managed_claude_profile_adopts_unowned_default_activity() {
        let mut config = FileConfig::default();
        config.providers.get_mut("claude").unwrap().profiles = vec![claude_profile(
            "managed",
            PathBuf::from("/profiles/managed"),
            vec![PathBuf::from("/profiles/managed/projects")],
        )];

        assert!(
            crate::providers::claude::adapter::assign_default_activity_owner(
                config.providers.get_mut("claude").unwrap()
            )
            .unwrap()
        );
        assert!(
            crate::providers::claude::settings::profile(&config.providers["claude"].profiles[0])
                .unwrap()
                .owns_default_claude_activity
        );
        assert!(
            !crate::providers::claude::adapter::assign_default_activity_owner(
                config.providers.get_mut("claude").unwrap()
            )
            .unwrap()
        );
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
            codex_profile("personal", personal_home),
            codex_profile("work", work_home),
        ];

        assert!(
            crate::providers::codex::adapter::assign_default_activity_owner_for_home(
                config.providers.get_mut("codex").unwrap(),
                &local_home
            )
            .unwrap()
        );
        assert!(
            crate::providers::codex::settings::profile(&config.providers["codex"].profiles[0])
                .unwrap()
                .owns_default_codex_activity
        );
        assert!(
            !crate::providers::codex::settings::profile(&config.providers["codex"].profiles[1])
                .unwrap()
                .owns_default_codex_activity
        );

        let persisted: FileConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert!(
            crate::providers::codex::settings::profile(&persisted.providers["codex"].profiles[0])
                .unwrap()
                .owns_default_codex_activity
        );
        assert!(
            !crate::providers::codex::settings::profile(&persisted.providers["codex"].profiles[1])
                .unwrap()
                .owns_default_codex_activity
        );

        fs::write(
            local_home.join("auth.json"),
            r#"{"tokens":{"account_id":"work"}}"#,
        )
        .unwrap();
        assert!(
            !crate::providers::codex::adapter::assign_default_activity_owner_for_home(
                config.providers.get_mut("codex").unwrap(),
                &local_home
            )
            .unwrap()
        );
        assert!(
            crate::providers::codex::settings::profile(&config.providers["codex"].profiles[0])
                .unwrap()
                .owns_default_codex_activity
        );
        assert!(
            !crate::providers::codex::settings::profile(&config.providers["codex"].profiles[1])
                .unwrap()
                .owns_default_codex_activity
        );
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
            codex_profile("default", local_home.clone()),
            codex_profile("managed", managed_home),
        ];

        assert!(
            !crate::providers::codex::adapter::assign_default_activity_owner_for_home(
                config.providers.get_mut("codex").unwrap(),
                &local_home
            )
            .unwrap()
        );
        assert!(config.providers["codex"].profiles.iter().all(|profile| {
            !crate::providers::codex::settings::profile(profile)
                .unwrap()
                .owns_default_codex_activity
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multiple_managed_claude_profiles_never_share_default_activity() {
        let mut config = FileConfig::default();
        config.providers.get_mut("claude").unwrap().profiles = vec![
            claude_profile("personal", PathBuf::from("/profiles/personal"), Vec::new()),
            claude_profile("work", PathBuf::from("/profiles/work"), Vec::new()),
        ];

        assert!(
            !crate::providers::claude::adapter::assign_default_activity_owner(
                config.providers.get_mut("claude").unwrap()
            )
            .unwrap()
        );
        assert!(config.providers["claude"].profiles.iter().all(|profile| {
            !crate::providers::claude::settings::profile(profile)
                .unwrap()
                .owns_default_claude_activity
        }));
    }

    #[test]
    fn loading_config_persists_default_claude_activity_owner_migration() {
        let root = std::env::temp_dir().join(format!("usage-config-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let mut file_config = FileConfig::default();
        file_config.providers.get_mut("claude").unwrap().profiles = vec![claude_profile(
            "managed",
            root.join("profiles/managed"),
            vec![root.join("profiles/managed/projects")],
        )];
        write_config_atomically(&config_path, &file_config).unwrap();

        let loaded = Config::load(
            Some(config_path.clone()),
            Some(root.join("usage.sqlite3")),
            Some(root.join("usage.sock")),
        )
        .unwrap();
        let persisted: FileConfig =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert!(
            crate::providers::claude::settings::profile(&loaded.providers["claude"].profiles[0])
                .unwrap()
                .owns_default_claude_activity
        );
        assert!(
            crate::providers::claude::settings::profile(&persisted.providers["claude"].profiles[0])
                .unwrap()
                .owns_default_claude_activity
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_null_provider_fields_are_removed_before_adapter_decoding() {
        let root = std::env::temp_dir().join(format!("usage-config-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &config_path,
            r#"{
                "poll_interval_seconds": 300,
                "providers": {
                    "codex": {"enabled": true, "cookie_header": null, "workspace_id": null},
                    "claude": {"enabled": false, "cookie_header": null, "workspace_id": null},
                    "opencode_go": {"enabled": false, "cookie_header": null, "workspace_id": null},
                    "grok": {"enabled": false, "cookie_header": null, "workspace_id": null}
                }
            }"#,
        )
        .unwrap();

        let loaded = Config::load_fixture(
            Some(config_path.clone()),
            Some(root.join("usage.sqlite3")),
            Some(root.join("usage.sock")),
        )
        .unwrap();

        assert_eq!(
            crate::runtime::provider_registry::build_collectors(&loaded)
                .unwrap()
                .len(),
            5
        );
        let persisted: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        for provider in ["codex", "claude", "cursor", "opencode_go", "grok"] {
            let provider = &persisted["providers"][provider];
            assert!(provider.get("cookie_header").is_none());
            assert!(provider.get("workspace_id").is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_non_null_provider_settings_are_removed_without_bricking_startup() {
        let root = std::env::temp_dir().join(format!("usage-config-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &config_path,
            r#"{
                "poll_interval_seconds": 300,
                "providers": {
                    "codex": {
                        "enabled": true,
                        "workspace_id": "stale",
                        "profiles": [{"id": "default", "cookie_header": "stale"}]
                    },
                    "claude": {"enabled": false, "source_mode": "stale"},
                    "opencode_go": {
                        "enabled": false,
                        "workspace_id": "wrk_valid",
                        "source_mode": "stale"
                    },
                    "grok": {"enabled": false, "workspace_id": "stale"}
                }
            }"#,
        )
        .unwrap();

        let loaded = Config::load_fixture(
            Some(config_path.clone()),
            Some(root.join("usage.sqlite3")),
            Some(root.join("usage.sock")),
        )
        .unwrap();

        assert_eq!(
            crate::runtime::provider_registry::build_collectors(&loaded)
                .unwrap()
                .len(),
            5
        );
        let persisted: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(persisted["providers"]["codex"]
            .get("workspace_id")
            .is_none());
        assert!(persisted["providers"]["codex"]["profiles"][0]
            .get("cookie_header")
            .is_none());
        assert!(persisted["providers"]["claude"]
            .get("source_mode")
            .is_none());
        assert_eq!(
            persisted["providers"]["opencode_go"]["workspace_id"],
            "wrk_valid"
        );
        assert!(persisted["providers"]["opencode_go"]
            .get("source_mode")
            .is_none());
        assert!(persisted["providers"]["grok"].get("workspace_id").is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
