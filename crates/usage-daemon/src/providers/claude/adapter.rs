use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use tracing::info;
use usage_core::{
    Account, AccountId, AccountImportPreview, AccountLaunchSettingsResponse,
    AddProviderAccountResponse, ImportJob, ImportJobId, ImportJobStatus, ImportMode, ImportOptions,
    ImportToggleSize, ProviderActionResponse, ProviderId,
};

use crate::{
    config::ProviderConfig,
    daemon::DaemonRuntime,
    providers::{
        launchers,
        paths::expand_home_path,
        profile_service::{
            create_managed_claude_profile, ensure_claude_login_profile, pending_claude_profile,
            ClaudeLoginTarget,
        },
        ProviderCollector,
    },
    runtime::{
        managed_profiles,
        provider_adapter::{
            plan_profile_deletion, AccountDeletionPlan, AddAccountHandler, DeleteHandler,
            ExecutionPolicy, ImportHandler, InvalidImportRequest, InvalidLaunchRequest,
            LaunchHandler, LaunchOverrides, LocalUsagePathMatcher, LocalUsageWatch,
            ProviderAdapter, ProviderManifest, ProviderRuntime, RepairHandler,
        },
    },
};

use super::{local_import, settings, ClaudeCollector, PROVIDER_ID};

pub(crate) static ADAPTER: ClaudeAdapter = ClaudeAdapter;

pub(crate) struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: PROVIDER_ID,
            display_name: "Claude",
            minimum_refresh_interval_seconds: 60,
            default_visible: false,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(Duration::from_secs(30), Duration::from_secs(75), 2)
    }

    fn profile_setting_keys(&self) -> &'static [&'static str] {
        &[
            "keychain_account",
            "keychain_service",
            "credentials_file",
            "claude_config_dir",
            "cli_enabled",
            "project_roots",
            "owns_default_claude_activity",
            "working_directory",
            "launch",
            "local_import",
        ]
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn local_usage_watch(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Option<LocalUsageWatch>> {
        let mut roots = Vec::new();
        if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
            roots.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| PathBuf::from(value).join("projects")),
            );
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".config/claude/projects"));
            roots.push(home.join(".claude/projects"));
        }

        let managed_root =
            usage_core::default_app_dir().map(|root| root.join("profiles").join(PROVIDER_ID));
        if let Some(root) = managed_root.as_ref() {
            roots.push(root.clone());
        }
        for profile in config
            .profiles
            .iter()
            .filter(|profile| profile.enabled && !profile.deleted)
        {
            let settings = settings::profile(profile)?;
            let configured = if settings.project_roots.is_empty() {
                settings
                    .claude_config_dir
                    .as_ref()
                    .map(|root| vec![expand_home_path(root).join("projects")])
                    .unwrap_or_default()
            } else {
                settings
                    .project_roots
                    .iter()
                    .map(expand_home_path)
                    .collect()
            };
            roots.extend(configured.into_iter().filter(|root| {
                managed_root
                    .as_ref()
                    .is_none_or(|managed| !root.starts_with(managed))
            }));
        }
        roots.sort();
        roots.dedup();
        Ok(Some(LocalUsageWatch::new(
            roots,
            [LocalUsagePathMatcher::extension("jsonl")],
            Duration::from_secs(60),
        )))
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(ClaudeCollector::new(config.clone())?))
    }

    fn migrate_config(
        &self,
        config: &mut ProviderConfig,
        discover_local_activity_owners: bool,
    ) -> anyhow::Result<bool> {
        if discover_local_activity_owners {
            assign_default_activity_owner(config)
        } else {
            Ok(false)
        }
    }

    fn add_account_handler(&self) -> Option<&dyn AddAccountHandler> {
        Some(self)
    }

    fn repair_handler(&self) -> Option<&dyn RepairHandler> {
        Some(self)
    }

    fn launch_handler(&self) -> Option<&dyn LaunchHandler> {
        Some(self)
    }

    fn import_handler(&self) -> Option<&dyn ImportHandler> {
        Some(self)
    }

    fn delete_handler(&self) -> Option<&dyn DeleteHandler> {
        Some(self)
    }
}

pub(crate) fn assign_default_activity_owner(config: &mut ProviderConfig) -> anyhow::Result<bool> {
    let active = config
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.enabled && !profile.deleted)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if active.len() != 1 {
        return Ok(false);
    }
    let profile_settings = settings::profile(&config.profiles[active[0]])?;
    if profile_settings.owns_default_claude_activity
        || profile_settings.claude_config_dir.is_none()
        || profile_settings.project_roots.iter().any(|root| {
            let value = root.to_string_lossy();
            value.ends_with("/.claude/projects") || value.ends_with("/.config/claude/projects")
        })
    {
        return Ok(false);
    }
    settings::update_profile(&mut config.profiles[active[0]], |settings| {
        settings.owns_default_claude_activity = true;
    })?;
    Ok(true)
}

#[async_trait]
impl AddAccountHandler for ClaudeAdapter {
    async fn add_account(
        &self,
        runtime: ProviderRuntime<'_>,
        display_name: Option<String>,
    ) -> anyhow::Result<AddProviderAccountResponse> {
        let connected_profiles = runtime
            .storage()
            .accounts()
            .await?
            .into_iter()
            .filter(|account| account.provider_id.as_str() == PROVIDER_ID)
            .filter_map(|account| account.profile_id)
            .collect::<BTreeSet<_>>();
        let target = runtime
            .mutate_config(|config| {
                let provider = config.providers.entry(PROVIDER_ID.to_string()).or_default();
                provider.enabled = true;
                pending_claude_profile(provider, &connected_profiles)
                    .map(Ok)
                    .unwrap_or_else(|| {
                        create_managed_claude_profile(provider, display_name.clone())
                    })
            })
            .await?;

        let profile_path = target
            .config_dir
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed Claude profile is missing its config path"))?;
        let login = launchers::launch_claude_login(Some(&profile_path))?;
        let authentication_url = login.authentication_url.clone();
        launchers::monitor_login(
            login.child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(target.profile_id.clone()),
        );
        info!(
            provider_id = PROVIDER_ID,
            profile_id = target.profile_id.as_str(),
            profile_path = %profile_path.display(),
            "provider account login launched"
        );
        Ok(AddProviderAccountResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            profile_id: target.profile_id,
            display_name: target.display_name,
            profile_path: profile_path.display().to_string(),
            authentication_url,
        })
    }
}

#[async_trait]
impl RepairHandler for ClaudeAdapter {
    async fn repair(
        &self,
        runtime: ProviderRuntime<'_>,
        account_id: Option<AccountId>,
    ) -> anyhow::Result<ProviderActionResponse> {
        let target = prepare_login_profile(runtime, account_id.as_ref()).await?;
        let login = launchers::launch_claude_login(target.config_dir.as_deref())?;
        let authentication_url = login.authentication_url.clone();
        launchers::monitor_login(
            login.child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(target.profile_id),
        );
        Ok(ProviderActionResponse {
            provider_id: ProviderId::new(PROVIDER_ID),
            message: "Finish signing in to Claude in your browser. UsageTracker will refresh automatically."
                .to_string(),
            authentication_url,
        })
    }
}

pub(crate) struct LaunchPlan {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) flags: Option<usage_core::LaunchFlags>,
    pub(crate) persist: Option<PersistedLaunchPrefs>,
}

fn is_active_profile(profile: &crate::config::ProviderProfileConfig, profile_id: &str) -> bool {
    profile.enabled && !profile.deleted && profile.id.as_deref() == Some(profile_id)
}

/// When present in a LaunchPlan, replaces both saved fields wholesale on a
/// successful open; a None field here clears the saved value.
pub(crate) struct PersistedLaunchPrefs {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) launch: Option<usage_core::LaunchFlags>,
}

/// Merges saved prefs with per-open overrides. Working directory + model +
/// effort persist whenever the sheet sent anything; the dangerous flag
/// persists only when explicitly remembered.
pub(crate) fn resolve_launch_plan(
    saved: &settings::ClaudeProfileSettings,
    overrides: &LaunchOverrides,
) -> anyhow::Result<LaunchPlan> {
    let working_directory = match &overrides.working_directory {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(PathBuf::from(value.trim())),
        None => saved.working_directory.clone(),
    };
    let flags = overrides.launch.clone().or_else(|| saved.launch.clone());
    if let Some(flags) = &flags {
        settings::validate_launch_flags(flags)?;
    }
    let persist =
        (overrides.working_directory.is_some() || overrides.launch.is_some()).then(|| {
            let mut launch = flags.clone();
            if !overrides.remember_dangerously_skip_permissions {
                if let Some(launch) = launch.as_mut() {
                    launch.dangerously_skip_permissions = saved
                        .launch
                        .as_ref()
                        .is_some_and(|saved| saved.dangerously_skip_permissions);
                }
            }
            PersistedLaunchPrefs {
                working_directory: working_directory.clone(),
                launch: launch.filter(|flags| flags != &usage_core::LaunchFlags::default()),
            }
        });
    Ok(LaunchPlan {
        working_directory,
        flags,
        persist,
    })
}

pub(crate) fn account_launch_settings_response(
    account: &Account,
    settings: &settings::ClaudeProfileSettings,
) -> AccountLaunchSettingsResponse {
    let has_managed_config_dir = settings.claude_config_dir.as_ref().is_some_and(|dir| {
        managed_profiles::is_managed_profile(&expand_home_path(dir), PROVIDER_ID)
    });
    AccountLaunchSettingsResponse {
        provider_id: account.provider_id.clone(),
        account_id: account.id.clone(),
        working_directory: settings
            .working_directory
            .as_ref()
            .map(|path| path.display().to_string()),
        launch: settings.launch.clone(),
        has_managed_config_dir,
    }
}

#[async_trait]
impl LaunchHandler for ClaudeAdapter {
    async fn launch(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
        overrides: LaunchOverrides,
    ) -> anyhow::Result<ProviderActionResponse> {
        if !account.collection_enabled {
            anyhow::bail!("enable Claude account tracking before opening a profile session");
        }
        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = runtime.config().await;
        let provider = config
            .providers
            .get(PROVIDER_ID)
            .ok_or_else(|| anyhow::anyhow!("Claude is not configured"))?;
        let (config_dir, saved, has_profile_entry) =
            if provider.profiles.is_empty() && profile_id == "default" {
                (None, settings::ClaudeProfileSettings::default(), false)
            } else {
                let profile = provider
                    .profiles
                    .iter()
                    .find(|profile| is_active_profile(profile, profile_id))
                    .ok_or_else(|| {
                        anyhow::anyhow!("Claude profile {profile_id} is no longer configured")
                    })?;
                let saved = settings::profile(profile)?;
                let config_dir = saved
                    .claude_config_dir
                    .clone()
                    .map(|dir| expand_home_path(&dir));
                (config_dir, saved, true)
            };

        let plan = resolve_launch_plan(&saved, &overrides)
            .map_err(|err| anyhow::Error::new(InvalidLaunchRequest(err.to_string())))?;
        let working_directory = match &plan.working_directory {
            Some(dir) => {
                let expanded = expand_home_path(dir);
                // Absolute is required: zsh treats `cd -- '-'` as $OLDPWD, and a
                // relative path is meaningless in a launcher run from an
                // arbitrary Terminal cwd.
                if !expanded.is_absolute() {
                    return Err(InvalidLaunchRequest(format!(
                        "working directory {} must be an absolute path",
                        expanded.display()
                    ))
                    .into());
                }
                if !expanded.is_dir() {
                    return Err(InvalidLaunchRequest(format!(
                        "working directory {} does not exist",
                        expanded.display()
                    ))
                    .into());
                }
                Some(expanded)
            }
            None => None,
        };

        let launcher = launchers::write_claude_profile_launcher(
            &account.id,
            config_dir.as_deref(),
            working_directory.as_deref(),
            plan.flags.as_ref(),
        )?;
        launchers::open_terminal(&launcher)?;

        if let (Some(persist), true) = (plan.persist, has_profile_entry) {
            let unchanged = persist.working_directory == saved.working_directory
                && persist.launch == saved.launch;
            // Unchanged prefs skip the config rewrite (and collector rebuild) entirely.
            if !unchanged {
                runtime
                    .mutate_config(|config| {
                        let Some(provider) = config.providers.get_mut(PROVIDER_ID) else {
                            return Ok(());
                        };
                        if let Some(profile) = provider
                            .profiles
                            .iter_mut()
                            .find(|profile| is_active_profile(profile, profile_id))
                        {
                            settings::update_profile(profile, |settings| {
                                settings.working_directory = persist.working_directory.clone();
                                settings.launch = persist.launch.clone();
                            })?;
                        }
                        Ok(())
                    })
                    .await
                    .context("the Claude session opened, but saving launch preferences failed")?;
            }
        }

        Ok(ProviderActionResponse {
            provider_id: account.provider_id,
            message: format!(
                "Opened a Claude session for {}. Activity from this terminal stays with this profile.",
                account.display_name.as_deref().unwrap_or(profile_id)
            ),
            authentication_url: None,
        })
    }

    fn supports_launch_options(&self) -> bool {
        true
    }

    /// Unresolvable profiles intentionally fall back to defaults so the read
    /// path never blocks the sheet; a confirm still surfaces the real launch
    /// error.
    async fn launch_settings(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<AccountLaunchSettingsResponse> {
        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = runtime.config().await;
        let saved = config
            .providers
            .get(PROVIDER_ID)
            .and_then(|provider| {
                provider
                    .profiles
                    .iter()
                    .find(|profile| is_active_profile(profile, profile_id))
            })
            .map(settings::profile)
            .transpose()?
            .unwrap_or_default();
        Ok(account_launch_settings_response(&account, &saved))
    }
}

#[async_trait]
impl DeleteHandler for ClaudeAdapter {
    fn plan_deletion(
        &self,
        config: &crate::config::Config,
        account: &Account,
    ) -> anyhow::Result<AccountDeletionPlan> {
        let profile_id = account.profile_id.as_deref();
        plan_profile_deletion(
            config,
            account,
            |_, profile| profile.id.as_deref() == profile_id,
            |profile| Ok(settings::profile(profile)?.claude_config_dir),
        )
    }
}

/// Full toggle catalog for the import preview. Order matches the design table
/// (comfort defaults first, stretch toggles last) so the Swift sheet can render
/// the list without additional sorting.
const IMPORT_TOGGLE_KEYS: &[&str] = &[
    "prefs",
    "project_trust",
    "prompt_history",
    "plugins",
    "project_transcripts",
    "file_history",
    "tasks_teams",
    "sessions",
];

fn source_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory could not be resolved"))?;
    Ok((home.join(".claude"), home.join(".claude.json")))
}

/// Loads the (managed or manual) profile settings for `profile_id`, falling
/// back to defaults for the legacy pre-managed-profile Claude account.
fn load_profile_settings(
    config: &crate::config::Config,
    profile_id: &str,
) -> anyhow::Result<settings::ClaudeProfileSettings> {
    let provider = match config.providers.get(PROVIDER_ID) {
        Some(provider) => provider,
        None => return Ok(settings::ClaudeProfileSettings::default()),
    };
    if provider.profiles.is_empty() && profile_id == "default" {
        return Ok(settings::ClaudeProfileSettings::default());
    }
    provider
        .profiles
        .iter()
        .find(|profile| is_active_profile(profile, profile_id))
        .map(settings::profile)
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("Claude profile {profile_id} is no longer configured"))
}

fn build_toggle_list(
    default_options: &ImportOptions,
    estimates: &HashMap<String, Option<u64>>,
) -> Vec<ImportToggleSize> {
    IMPORT_TOGGLE_KEYS
        .iter()
        .map(|key| toggle_entry(key, default_options, estimates))
        .collect()
}

fn toggle_entry(
    key: &str,
    default_options: &ImportOptions,
    estimates: &HashMap<String, Option<u64>>,
) -> ImportToggleSize {
    let (enabled_by_default, supported, note) = match key {
        "prefs" => (default_options.prefs, true, None),
        "project_trust" => (default_options.project_trust, true, None),
        "prompt_history" => (default_options.prompt_history, true, None),
        "plugins" => (
            false,
            false,
            Some("plugins require a path rewrite and are not imported yet".to_string()),
        ),
        "project_transcripts" => (
            false,
            false,
            Some("transcripts are excluded to preserve usage attribution".to_string()),
        ),
        "file_history" => (
            false,
            false,
            Some("file-history import is not supported yet".to_string()),
        ),
        "tasks_teams" => (
            false,
            false,
            Some("tasks/teams import is not supported yet".to_string()),
        ),
        "sessions" => (
            false,
            false,
            Some("sessions import is not supported yet".to_string()),
        ),
        other => (false, false, Some(format!("unknown toggle {other}"))),
    };
    ImportToggleSize {
        key: key.to_string(),
        enabled_by_default,
        supported,
        bytes: estimates.get(key).and_then(|value| *value),
        note,
    }
}

#[async_trait]
impl ImportHandler for ClaudeAdapter {
    async fn preview(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<AccountImportPreview> {
        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = runtime.config().await;
        let saved = load_profile_settings(&config, profile_id)?;

        let destination_path = saved.claude_config_dir.as_deref().map(expand_home_path);
        let has_managed_config_dir = destination_path
            .as_ref()
            .is_some_and(|dir| managed_profiles::is_managed_profile(dir, PROVIDER_ID));

        let (source_home, source_claude_json) = source_paths()?;
        let default_options = ImportOptions::comfort_defaults();
        // estimate_toggle_bytes only reports currently-selectable toggles; the
        // stretch entries fall through `toggle_entry` with `bytes = None`.
        let estimates: HashMap<String, Option<u64>> = local_import::estimate_toggle_bytes(
            &source_home,
            &source_claude_json,
            &default_options,
        )
        .into_iter()
        .collect();
        let toggles = build_toggle_list(&default_options, &estimates);
        let source_identity = local_import::read_source_identity(&source_claude_json);

        Ok(AccountImportPreview {
            provider_id: account.provider_id,
            account_id: account.id,
            source_home: source_home.display().to_string(),
            source_claude_json: source_claude_json.display().to_string(),
            destination: destination_path
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
            has_managed_config_dir,
            source_identity,
            default_mode: ImportMode::PrefsOnly,
            default_options,
            toggles,
        })
    }

    async fn start_import(
        &self,
        runtime: Arc<DaemonRuntime>,
        account: Account,
        options: ImportOptions,
        mode: ImportMode,
    ) -> anyhow::Result<ImportJob> {
        options
            .ensure_pr2_supported()
            .map_err(|reason| anyhow::Error::new(InvalidImportRequest(reason)))?;

        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?
            .to_string();

        let config = runtime.config_snapshot().await;
        let saved = load_profile_settings(&config, &profile_id)?;
        let destination = saved
            .claude_config_dir
            .as_deref()
            .map(expand_home_path)
            .ok_or_else(|| {
                anyhow::Error::new(InvalidImportRequest(
                    "importing requires a managed Claude config directory".to_string(),
                ))
            })?;
        if !managed_profiles::is_managed_profile(&destination, PROVIDER_ID) {
            return Err(anyhow::Error::new(InvalidImportRequest(
                "importing is only supported for managed Claude profiles".to_string(),
            )));
        }

        let (source_home, source_claude_json) = source_paths()?;
        let plan = local_import::plan(&source_home, &source_claude_json, &options)?;
        let source_identity = plan.source_identity.clone();

        let job = ImportJob {
            id: ImportJobId::new(uuid::Uuid::new_v4().to_string()),
            account_id: account.id.clone(),
            provider_id: account.provider_id.clone(),
            status: ImportJobStatus::Queued,
            mode,
            options: options.clone(),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            progress_message: None,
            failure_message: None,
        };

        let import_jobs = runtime.import_jobs.clone();
        let runtime_for_persist = runtime.clone();
        let source_home_display = source_home.display().to_string();

        import_jobs
            .start(job, move || async move {
                // GB-scale filesystem work never runs on the async worker
                // pool — even prefs-only imports go through spawn_blocking so
                // one long stat/copy cannot stall the daemon socket.
                let manifest = tokio::task::spawn_blocking(move || {
                    local_import::run(&plan, &destination, mode)
                })
                .await
                .map_err(|err| anyhow::anyhow!("import worker task failed: {err}"))??;
                persist_local_import(
                    runtime_for_persist,
                    profile_id,
                    options,
                    manifest,
                    source_home_display,
                    source_identity,
                )
                .await
            })
            .await
    }
}

async fn persist_local_import(
    runtime: Arc<DaemonRuntime>,
    profile_id: String,
    options: ImportOptions,
    manifest: local_import::ImportManifest,
    source: String,
    source_identity: Option<String>,
) -> anyhow::Result<()> {
    runtime
        .mutate_config(|config| {
            let Some(provider) = config.providers.get_mut(PROVIDER_ID) else {
                return Ok(());
            };
            let Some(profile) = provider
                .profiles
                .iter_mut()
                .find(|profile| is_active_profile(profile, &profile_id))
            else {
                return Ok(());
            };
            settings::update_profile(profile, |profile_settings| {
                profile_settings.local_import = Some(settings::ClaudeLocalImportSettings {
                    last_imported_at: Some(manifest.imported_at),
                    source: Some(source.clone()),
                    source_identity: source_identity.clone(),
                    options: Some(options.clone()),
                    manifest: Some(settings::ClaudeImportManifest {
                        paths: manifest.paths.clone(),
                        imported_at: manifest.imported_at,
                    }),
                });
            })?;
            Ok(())
        })
        .await
        .context("the Claude import completed, but saving import bookkeeping failed")
}

async fn prepare_login_profile(
    runtime: ProviderRuntime<'_>,
    account_id: Option<&AccountId>,
) -> anyhow::Result<ClaudeLoginTarget> {
    let requested_profile_id = match account_id {
        Some(account_id) => runtime
            .storage()
            .account(account_id)
            .await?
            .and_then(|account| account.profile_id),
        None => None,
    };
    runtime
        .mutate_config(|config| {
            let provider = config.providers.entry(PROVIDER_ID.to_string()).or_default();
            provider.enabled = true;
            ensure_claude_login_profile(provider, requested_profile_id.as_deref())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProfileConfig;

    fn saved_settings(
        working_directory: Option<&str>,
        launch: Option<usage_core::LaunchFlags>,
    ) -> settings::ClaudeProfileSettings {
        settings::ClaudeProfileSettings {
            working_directory: working_directory.map(PathBuf::from),
            launch,
            ..Default::default()
        }
    }

    #[test]
    fn launch_plan_uses_saved_prefs_when_no_overrides_arrive() {
        let saved = saved_settings(
            Some("/tmp/saved"),
            Some(usage_core::LaunchFlags {
                model: Some("fable".to_string()),
                ..Default::default()
            }),
        );
        let plan = resolve_launch_plan(&saved, &LaunchOverrides::default()).unwrap();
        assert_eq!(plan.working_directory, Some(PathBuf::from("/tmp/saved")));
        assert_eq!(plan.flags.as_ref().unwrap().model.as_deref(), Some("fable"));
        // A plain open never rewrites config.
        assert!(plan.persist.is_none());
    }

    #[test]
    fn launch_plan_overrides_replace_whole_flag_object_and_persist() {
        let saved = saved_settings(
            Some("/tmp/saved"),
            Some(usage_core::LaunchFlags {
                model: Some("old-model".to_string()),
                effort: Some(usage_core::LaunchEffort::Low),
                dangerously_skip_permissions: false,
            }),
        );
        let overrides = LaunchOverrides {
            working_directory: Some("/tmp/override".to_string()),
            launch: Some(usage_core::LaunchFlags {
                model: None,
                effort: Some(usage_core::LaunchEffort::Max),
                dangerously_skip_permissions: true,
            }),
            remember_dangerously_skip_permissions: false,
        };
        let plan = resolve_launch_plan(&saved, &overrides).unwrap();
        // This run uses the override verbatim (dangerous flag on, once).
        assert_eq!(plan.working_directory, Some(PathBuf::from("/tmp/override")));
        let flags = plan.flags.as_ref().unwrap();
        assert_eq!(flags.model, None);
        assert!(flags.dangerously_skip_permissions);
        // Persisted prefs keep model/effort/cwd but reset the dangerous flag
        // to its saved value because remember was not requested.
        let persist = plan.persist.unwrap();
        assert_eq!(
            persist.working_directory,
            Some(PathBuf::from("/tmp/override"))
        );
        let persisted_flags = persist.launch.unwrap();
        assert_eq!(persisted_flags.effort, Some(usage_core::LaunchEffort::Max));
        assert!(!persisted_flags.dangerously_skip_permissions);
        assert_eq!(persisted_flags.model, None);
    }

    #[test]
    fn launch_plan_remembers_the_dangerous_flag_only_on_request() {
        let saved = saved_settings(None, None);
        let overrides = LaunchOverrides {
            working_directory: None,
            launch: Some(usage_core::LaunchFlags {
                dangerously_skip_permissions: true,
                ..Default::default()
            }),
            remember_dangerously_skip_permissions: true,
        };
        let plan = resolve_launch_plan(&saved, &overrides).unwrap();
        assert!(
            plan.persist
                .unwrap()
                .launch
                .unwrap()
                .dangerously_skip_permissions
        );
    }

    #[test]
    fn launch_plan_forgetting_the_dangerous_flag_normalizes_to_no_persisted_flags() {
        let saved = saved_settings(None, None);
        let overrides = LaunchOverrides {
            working_directory: None,
            launch: Some(usage_core::LaunchFlags {
                dangerously_skip_permissions: true,
                ..Default::default()
            }),
            remember_dangerously_skip_permissions: false,
        };
        let plan = resolve_launch_plan(&saved, &overrides).unwrap();
        // The dangerous flag resets to the (unset) saved value, which
        // collapses the persisted flags back down to "nothing saved".
        assert_eq!(plan.persist.unwrap().launch, None);
    }

    #[test]
    fn launch_plan_clears_prefs_and_rejects_invalid_models() {
        let saved = saved_settings(Some("/tmp/saved"), None);
        // An empty working directory from the sheet clears the saved value.
        let cleared = resolve_launch_plan(
            &saved,
            &LaunchOverrides {
                working_directory: Some("   ".to_string()),
                launch: Some(usage_core::LaunchFlags::default()),
                remember_dangerously_skip_permissions: false,
            },
        )
        .unwrap();
        assert_eq!(cleared.working_directory, None);
        let persist = cleared.persist.unwrap();
        assert_eq!(persist.working_directory, None);
        // All-default flags normalize to "no flags saved".
        assert_eq!(persist.launch, None);

        // A padded working directory is trimmed once at acceptance, not left
        // to trip the launch handler's absolute-path check later.
        let padded = resolve_launch_plan(
            &saved,
            &LaunchOverrides {
                working_directory: Some("  /tmp/padded  ".to_string()),
                launch: None,
                remember_dangerously_skip_permissions: false,
            },
        )
        .unwrap();
        assert_eq!(padded.working_directory, Some(PathBuf::from("/tmp/padded")));

        let invalid = resolve_launch_plan(
            &saved,
            &LaunchOverrides {
                working_directory: None,
                launch: Some(usage_core::LaunchFlags {
                    model: Some("bad model; rm".to_string()),
                    ..Default::default()
                }),
                remember_dangerously_skip_permissions: false,
            },
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn local_watch_roots_keep_managed_and_manual_profiles_separate() {
        let profile =
            |id: &str, enabled: bool, config_dir: Option<PathBuf>, roots: Vec<PathBuf>| {
                let mut profile = ProviderProfileConfig {
                    id: Some(id.to_string()),
                    enabled,
                    ..ProviderProfileConfig::default()
                };
                settings::update_profile(&mut profile, |settings| {
                    settings.claude_config_dir = config_dir;
                    settings.project_roots = roots;
                })
                .unwrap();
                profile
            };
        let config = ProviderConfig {
            enabled: true,
            profiles: vec![
                profile(
                    "managed",
                    true,
                    usage_core::default_app_dir().map(|root| root.join("profiles/claude/managed")),
                    Vec::new(),
                ),
                profile(
                    "manual",
                    true,
                    None,
                    vec![PathBuf::from("/tmp/manual-claude/projects")],
                ),
                profile(
                    "disabled",
                    false,
                    None,
                    vec![PathBuf::from("/tmp/disabled-claude/projects")],
                ),
            ],
            ..ProviderConfig::default()
        };

        let watch = ADAPTER.local_usage_watch(&config).unwrap().unwrap();

        assert!(watch
            .roots
            .contains(&PathBuf::from("/tmp/manual-claude/projects")));
        assert!(!watch
            .roots
            .contains(&PathBuf::from("/tmp/disabled-claude/projects")));
        if let Some(managed) =
            usage_core::default_app_dir().map(|root| root.join("profiles").join(PROVIDER_ID))
        {
            assert!(watch.roots.contains(&managed));
            assert!(!watch.roots.contains(&managed.join("managed/projects")));
        }
    }

    #[test]
    fn launch_settings_response_reports_managed_dir_and_prefs() {
        let now = chrono::Utc::now();
        let account = Account {
            id: usage_core::AccountId::new("account-1"),
            provider_id: ProviderId::new(PROVIDER_ID),
            external_account_id: "user@example.com".to_string(),
            profile_id: Some("work".to_string()),
            display_name: None,
            display_name_source: usage_core::AccountDisplayNameSource::Generated,
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: now,
            updated_at: now,
        };
        let managed_dir =
            usage_core::default_app_dir().map(|root| root.join("profiles/claude/work"));
        let settings = settings::ClaudeProfileSettings {
            claude_config_dir: managed_dir,
            working_directory: Some(PathBuf::from("/tmp/work")),
            launch: Some(usage_core::LaunchFlags {
                model: Some("fable".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let response = account_launch_settings_response(&account, &settings);
        assert_eq!(response.account_id.as_str(), "account-1");
        assert_eq!(response.working_directory.as_deref(), Some("/tmp/work"));
        assert_eq!(response.launch.unwrap().model.as_deref(), Some("fable"));
        assert!(response.has_managed_config_dir);

        let unmanaged =
            account_launch_settings_response(&account, &settings::ClaudeProfileSettings::default());
        assert!(!unmanaged.has_managed_config_dir);
        assert_eq!(unmanaged.working_directory, None);
    }

    #[test]
    fn profile_setting_keys_cover_every_serialized_settings_field() {
        // Dual-registration guard: a field missing from profile_setting_keys()
        // is silently stripped and persisted away at the next config load.
        let settings = settings::ClaudeProfileSettings {
            keychain_account: Some("user".to_string()),
            keychain_service: Some("service".to_string()),
            credentials_file: Some(PathBuf::from("/tmp/credentials.json")),
            claude_config_dir: Some(PathBuf::from("/tmp/profile")),
            cli_enabled: Some(true),
            project_roots: vec![PathBuf::from("/tmp/projects")],
            owns_default_claude_activity: true,
            working_directory: Some(PathBuf::from("/tmp/work")),
            launch: Some(usage_core::LaunchFlags {
                model: Some("fable".to_string()),
                effort: Some(usage_core::LaunchEffort::Xhigh),
                dangerously_skip_permissions: true,
            }),
            local_import: Some(settings::ClaudeLocalImportSettings {
                last_imported_at: Some(chrono::Utc::now()),
                source: Some("~/.claude".to_string()),
                source_identity: Some("user@example.com".to_string()),
                options: Some(usage_core::ImportOptions::comfort_defaults()),
                manifest: Some(settings::ClaudeImportManifest {
                    paths: vec!["settings.json".to_string()],
                    imported_at: chrono::Utc::now(),
                }),
            }),
        };
        let value = serde_json::to_value(&settings).unwrap();
        let serialized_fields: BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let declared_keys: BTreeSet<&str> =
            ADAPTER.profile_setting_keys().iter().copied().collect();
        assert_eq!(
            declared_keys, serialized_fields,
            "profile_setting_keys() must exactly match ClaudeProfileSettings serialized fields"
        );
    }
}
