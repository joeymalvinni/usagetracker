//! Pure planner/runner for the Claude comfort-pack local import (PR2 scope:
//! prefs, project trust, prompt history only). Wiring into the API surface
//! happens in later PR2 tasks.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use usage_core::{ImportMode, ImportOptions};

use super::client;

const SETTINGS_FILE_NAME: &str = "settings.json";
const HISTORY_FILE_NAME: &str = "history.jsonl";
const CLAUDE_JSON_FILE_NAME: &str = ".claude.json";
/// Internal bookkeeping only (not part of the public `ImportManifest`), so
/// `Replace` mode knows which previously-imported paths to clean up. Task 3/5
/// also plan to record the manifest in profile settings; keeping this sidecar
/// too is intentional dual bookkeeping until that lands, since this module
/// must stay self-contained and side-effect-free beyond `dest_config_dir`.
const MANIFEST_STATE_FILE_NAME: &str = ".claude-comfort-import-manifest.json";
const FREE_SPACE_HEADROOM_BYTES: u64 = 1024 * 1024;

const SETTINGS_ALLOWLIST: &[&str] = &[
    "model",
    "theme",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "switchModelsOnFlag",
];

const TRUST_TOP_LEVEL_ALLOWLIST: &[&str] = &["hasCompletedOnboarding", "lastOnboardingVersion"];
const TRUST_PROJECT_ALLOWLIST: &[&str] =
    &["hasTrustDialogAccepted", "hasCompletedProjectOnboarding"];

/// Destination paths that a `Replace` import must never delete, even if they
/// are absent from the freshly computed plan.
const PROTECTED_DEST_PATHS: &[&str] = &[".credentials.json", CLAUDE_JSON_FILE_NAME];

#[derive(Debug)]
pub(crate) struct ImportPlan {
    pub source_home: PathBuf,
    pub source_claude_json: PathBuf,
    pub options: ImportOptions,
    pub items: Vec<ImportItem>,
    pub source_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImportItemKind {
    Settings,
    Trust,
    History,
    Plugins,
    Projects,
    FileHistory,
    TasksTeams,
    Sessions,
}

#[derive(Clone, Debug)]
pub(crate) struct ImportItem {
    pub kind: ImportItemKind,
    pub relative_dest: PathBuf,
    pub estimated_bytes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ImportManifest {
    pub paths: Vec<String>,
    pub imported_at: DateTime<Utc>,
}

/// Builds an import plan for the PR2 comfort paths (settings, project trust,
/// prompt history). Never reads beyond the exact files it needs, and never
/// follows a symlink for any of them.
pub(crate) fn plan(
    source_home: &Path,
    source_claude_json: &Path,
    options: &ImportOptions,
) -> anyhow::Result<ImportPlan> {
    options
        .ensure_pr2_supported()
        .map_err(|reason| anyhow::anyhow!(reason))?;

    let mut items = Vec::new();

    if options.prefs {
        if let Some(estimated_bytes) = stat_source_file(&source_home.join(SETTINGS_FILE_NAME))? {
            items.push(ImportItem {
                kind: ImportItemKind::Settings,
                relative_dest: PathBuf::from(SETTINGS_FILE_NAME),
                estimated_bytes,
            });
        }
    }

    if options.prompt_history {
        if let Some(estimated_bytes) = stat_source_file(&source_home.join(HISTORY_FILE_NAME))? {
            items.push(ImportItem {
                kind: ImportItemKind::History,
                relative_dest: PathBuf::from(HISTORY_FILE_NAME),
                estimated_bytes,
            });
        }
    }

    if options.project_trust {
        if let Some(estimated_bytes) = stat_source_file(source_claude_json)? {
            items.push(ImportItem {
                kind: ImportItemKind::Trust,
                relative_dest: PathBuf::from(CLAUDE_JSON_FILE_NAME),
                estimated_bytes,
            });
        }
    }

    Ok(ImportPlan {
        source_home: source_home.to_path_buf(),
        source_claude_json: source_claude_json.to_path_buf(),
        options: options.clone(),
        items,
        source_identity: read_source_identity(source_claude_json),
    })
}

/// Runs a previously computed plan, writing scrubbed comfort-pack files into
/// `dest_config_dir` via staging-file-then-rename. `prefs_only` and `replace`
/// behave identically for the three PR2 files themselves; `replace`
/// additionally removes paths that a prior import wrote but the new plan no
/// longer selects (never touching credentials or paths outside that prior
/// manifest).
pub(crate) fn run(
    plan: &ImportPlan,
    dest_config_dir: &Path,
    mode: ImportMode,
) -> anyhow::Result<ImportManifest> {
    if plan.items.is_empty() {
        return Ok(ImportManifest {
            paths: Vec::new(),
            imported_at: Utc::now(),
        });
    }

    reject_symlink(dest_config_dir)?;
    std::fs::create_dir_all(dest_config_dir)?;

    let required_bytes = plan
        .items
        .iter()
        .map(|item| item.estimated_bytes)
        .fold(0u64, u64::saturating_add)
        .saturating_add(FREE_SPACE_HEADROOM_BYTES);
    let available_bytes = available_space(dest_config_dir)?;
    if available_bytes < required_bytes {
        anyhow::bail!(
            "insufficient free space at {}: need at least {required_bytes} bytes, {available_bytes} available",
            dest_config_dir.display()
        );
    }

    let mut imported_paths = Vec::new();
    for item in &plan.items {
        let imported = match item.kind {
            ImportItemKind::Settings => import_settings(&plan.source_home, dest_config_dir)?,
            ImportItemKind::History => import_history(&plan.source_home, dest_config_dir)?,
            ImportItemKind::Trust => import_trust(&plan.source_claude_json, dest_config_dir)?,
            ImportItemKind::Plugins
            | ImportItemKind::Projects
            | ImportItemKind::FileHistory
            | ImportItemKind::TasksTeams
            | ImportItemKind::Sessions => false,
        };
        if imported {
            imported_paths.push(item.relative_dest.to_string_lossy().into_owned());
        }
    }

    if imported_paths.is_empty() {
        anyhow::bail!("Claude comfort import failed: no selected items could be imported");
    }

    if mode == ImportMode::Replace {
        cleanup_stale_replace_paths(dest_config_dir, &imported_paths)?;
    }

    let manifest = ImportManifest {
        paths: imported_paths,
        imported_at: Utc::now(),
    };
    persist_manifest_state(dest_config_dir, &manifest.paths)?;
    Ok(manifest)
}

/// Byte-size estimate per toggle, for the import preview UI. `None` means the
/// underlying source file does not exist.
pub(crate) fn estimate_toggle_bytes(
    source_home: &Path,
    source_claude_json: &Path,
    options: &ImportOptions,
) -> Vec<(String, Option<u64>)> {
    let mut estimates = Vec::new();
    if options.prefs {
        estimates.push((
            "prefs".to_string(),
            stat_source_file(&source_home.join(SETTINGS_FILE_NAME))
                .ok()
                .flatten(),
        ));
    }
    if options.prompt_history {
        estimates.push((
            "prompt_history".to_string(),
            stat_source_file(&source_home.join(HISTORY_FILE_NAME))
                .ok()
                .flatten(),
        ));
    }
    if options.project_trust {
        estimates.push((
            "project_trust".to_string(),
            stat_source_file(source_claude_json).ok().flatten(),
        ));
    }
    estimates
}

/// Keeps only the comfort-pack UI preference keys; drops everything else,
/// including `env` (which routinely carries API keys) and other secret- or
/// permission-adjacent fields.
pub(crate) fn scrub_settings_json(raw: &Value) -> anyhow::Result<Value> {
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("settings.json is not a JSON object"))?;
    let mut scrubbed = serde_json::Map::new();
    for key in SETTINGS_ALLOWLIST {
        if let Some(value) = object.get(*key) {
            scrubbed.insert((*key).to_string(), value.clone());
        }
    }
    Ok(Value::Object(scrubbed))
}

/// Keeps only onboarding/trust bookkeeping fields from `.claude.json`; drops
/// `mcpServers`, prompt history snippets, and any other project data.
pub(crate) fn scrub_claude_json(raw: &Value) -> anyhow::Result<Value> {
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!(".claude.json is not a JSON object"))?;
    let mut scrubbed = serde_json::Map::new();
    for key in TRUST_TOP_LEVEL_ALLOWLIST {
        if let Some(value) = object.get(*key) {
            scrubbed.insert((*key).to_string(), value.clone());
        }
    }

    let mut scrubbed_projects = serde_json::Map::new();
    if let Some(projects) = object.get("projects").and_then(Value::as_object) {
        for (project_path, project) in projects {
            let Some(project_object) = project.as_object() else {
                continue;
            };
            let mut scrubbed_project = serde_json::Map::new();
            for key in TRUST_PROJECT_ALLOWLIST {
                if let Some(value) = project_object.get(*key) {
                    scrubbed_project.insert((*key).to_string(), value.clone());
                }
            }
            scrubbed_projects.insert(project_path.clone(), Value::Object(scrubbed_project));
        }
    }
    scrubbed.insert("projects".to_string(), Value::Object(scrubbed_projects));

    Ok(Value::Object(scrubbed))
}

fn import_settings(source_home: &Path, dest_config_dir: &Path) -> anyhow::Result<bool> {
    let Some(raw) = read_source_json(&source_home.join(SETTINGS_FILE_NAME))? else {
        return Ok(false);
    };
    let scrubbed = scrub_settings_json(&raw)?;
    write_staged_json(&dest_config_dir.join(SETTINGS_FILE_NAME), &scrubbed)?;
    Ok(true)
}

fn import_trust(source_claude_json: &Path, dest_config_dir: &Path) -> anyhow::Result<bool> {
    let Some(raw) = read_source_json(source_claude_json)? else {
        return Ok(false);
    };
    let scrubbed = scrub_claude_json(&raw)?;
    write_staged_json(&dest_config_dir.join(CLAUDE_JSON_FILE_NAME), &scrubbed)?;
    Ok(true)
}

fn import_history(source_home: &Path, dest_config_dir: &Path) -> anyhow::Result<bool> {
    let source_path = source_home.join(HISTORY_FILE_NAME);
    if stat_source_file(&source_path)?.is_none() {
        return Ok(false);
    }

    let dest_path = dest_config_dir.join(HISTORY_FILE_NAME);
    // Fixed `.importing` suffix per the brief (unlike the uuid-suffixed
    // staging names `write_staged_json` uses for settings/trust) — history is
    // copied, not read-then-rewritten, so a stable name is fine here.
    let staging_path = dest_config_dir.join(format!("{HISTORY_FILE_NAME}.importing"));
    let copy_result = std::fs::copy(&source_path, &staging_path);
    match copy_result {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&staging_path);
            return Ok(false);
        }
        Err(err) => {
            let _ = std::fs::remove_file(&staging_path);
            return Err(err.into());
        }
    }
    if let Err(err) = std::fs::rename(&staging_path, &dest_path) {
        let _ = std::fs::remove_file(&staging_path);
        return Err(err.into());
    }
    Ok(true)
}

fn read_source_json(path: &Path) -> anyhow::Result<Option<Value>> {
    if stat_source_file(path)?.is_none() {
        return Ok(None);
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn write_staged_json(dest_path: &Path, value: &Value) -> anyhow::Result<()> {
    reject_symlink(dest_path)?;
    let contents = serde_json::to_vec_pretty(value)?;
    let parent = dest_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = dest_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("import.json");
    let staging_path = parent.join(format!(".{file_name}.{}.importing", uuid::Uuid::new_v4()));

    if let Err(err) = std::fs::write(&staging_path, &contents) {
        let _ = std::fs::remove_file(&staging_path);
        return Err(err.into());
    }
    if let Err(err) = std::fs::rename(&staging_path, dest_path) {
        let _ = std::fs::remove_file(&staging_path);
        return Err(err.into());
    }
    Ok(())
}

/// Returns the file's byte length without ever following a symlink. `Ok(None)`
/// means the source is simply absent (fine — the toggle is just unavailable);
/// finding a symlink is treated as a potential escape attempt and rejected.
fn stat_source_file(path: &Path) -> anyhow::Result<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                anyhow::bail!("refusing to import symlinked path {}", path.display());
            }
            if !metadata.is_file() {
                anyhow::bail!("expected a regular file at {}", path.display());
            }
            Ok(Some(metadata.len()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn reject_symlink(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("refusing to write to symlinked path {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Best-effort only: the source `.claude.json` rarely carries an OAuth
/// identity block, and any failure here (missing file, unexpected shape)
/// simply means the import preview shows no source identity.
pub(crate) fn read_source_identity(source_claude_json: &Path) -> Option<String> {
    let bytes = std::fs::read(source_claude_json).ok()?;
    let identity = client::parse_cached_profile_identity(&bytes).ok()?;
    identity.email.or(Some(identity.account_id))
}

fn available_space(path: &Path) -> anyhow::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let existing = first_existing_ancestor(path);
    let c_path = CString::new(existing.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("destination path contains an interior nul byte"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat.f_frsize.saturating_mul(u64::from(stat.f_bavail)))
}

fn first_existing_ancestor(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    loop {
        if candidate.exists() {
            return candidate;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent.to_path_buf(),
            None => return PathBuf::from("/"),
        }
    }
}

fn cleanup_stale_replace_paths(
    dest_config_dir: &Path,
    current_paths: &[String],
) -> anyhow::Result<()> {
    let previous_paths = read_manifest_state(dest_config_dir);
    for path in previous_paths {
        if current_paths.contains(&path) || PROTECTED_DEST_PATHS.contains(&path.as_str()) {
            continue;
        }
        let full_path = dest_config_dir.join(&path);
        if let Ok(metadata) = std::fs::symlink_metadata(&full_path) {
            if metadata.is_file() {
                let _ = std::fs::remove_file(&full_path);
            }
        }
    }
    Ok(())
}

fn read_manifest_state(dest_config_dir: &Path) -> Vec<String> {
    let path = dest_config_dir.join(MANIFEST_STATE_FILE_NAME);
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
}

fn persist_manifest_state(dest_config_dir: &Path, paths: &[String]) -> anyhow::Result<()> {
    let value = serde_json::to_value(paths)?;
    write_staged_json(&dest_config_dir.join(MANIFEST_STATE_FILE_NAME), &value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{label}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn plan_includes_only_selected_comfort_paths() {
        let dir = ScratchDir::new("claude-local-import-plan");
        let home = dir.path.join("claude-home");
        let json = dir.path.join("claude.json");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("settings.json"),
            r#"{"theme":"dark","env":{"X":"1"}}"#,
        )
        .unwrap();
        fs::write(home.join("history.jsonl"), "line\n").unwrap();
        fs::write(
            &json,
            r#"{"projects":{"/tmp/demo":{"hasTrustDialogAccepted":true,"mcpServers":{}}}}"#,
        )
        .unwrap();

        let plan = plan(&home, &json, &ImportOptions::comfort_defaults()).unwrap();
        assert!(plan
            .items
            .iter()
            .any(|i| i.kind == ImportItemKind::Settings));
        assert!(plan.items.iter().any(|i| i.kind == ImportItemKind::History));
        assert!(plan.items.iter().any(|i| i.kind == ImportItemKind::Trust));
        assert!(!plan
            .items
            .iter()
            .any(|i| matches!(i.kind, ImportItemKind::Plugins | ImportItemKind::Projects)));
    }

    #[test]
    fn settings_scrub_drops_secrets_and_keeps_comfort_keys() {
        let raw = serde_json::json!({
            "theme": "dark",
            "model": "fable",
            "enabledPlugins": {"x@y": true},
            "env": {"ANTHROPIC_API_KEY": "secret"},
            "apiKeyHelper": "evil",
            "skipDangerousModePermissionPrompt": true
        });
        let scrubbed = scrub_settings_json(&raw).unwrap();
        assert_eq!(scrubbed["theme"], "dark");
        assert_eq!(scrubbed["model"], "fable");
        assert!(scrubbed.get("env").is_none());
        assert!(scrubbed.get("apiKeyHelper").is_none());
        assert!(scrubbed.get("skipDangerousModePermissionPrompt").is_none());
    }

    #[test]
    fn trust_scrub_keeps_only_allowlisted_project_keys() {
        let raw = serde_json::json!({
            "hasCompletedOnboarding": true,
            "projects": {
                "/tmp/demo": {
                    "hasTrustDialogAccepted": true,
                    "hasCompletedProjectOnboarding": true,
                    "mcpServers": {"bad": {}},
                    "lastSessionFirstPrompt": "hello"
                }
            }
        });
        let scrubbed = scrub_claude_json(&raw).unwrap();
        assert_eq!(scrubbed["hasCompletedOnboarding"], true);
        let project = &scrubbed["projects"]["/tmp/demo"];
        assert_eq!(project["hasTrustDialogAccepted"], true);
        assert!(project.get("mcpServers").is_none());
        assert!(project.get("lastSessionFirstPrompt").is_none());
    }

    #[test]
    fn run_prefs_only_writes_scrubbed_files_and_leaves_credentials() {
        let dir = ScratchDir::new("claude-local-import-run");
        let source_home = dir.path.join("src");
        let source_json = dir.path.join("src.json");
        let dest = dir.path.join("dest");
        fs::create_dir_all(&source_home).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(
            source_home.join("settings.json"),
            r#"{"theme":"dark","env":{"X":"1"}}"#,
        )
        .unwrap();
        fs::write(source_home.join("history.jsonl"), "prompt\n").unwrap();
        fs::write(
            &source_json,
            r#"{"hasCompletedOnboarding":true,"projects":{"/a":{"hasTrustDialogAccepted":true,"mcpServers":{}}}}"#,
        )
        .unwrap();
        let creds = br#"{"keep":"me"}"#;
        fs::write(dest.join(".credentials.json"), creds).unwrap();

        let plan = plan(
            &source_home,
            &source_json,
            &ImportOptions::comfort_defaults(),
        )
        .unwrap();
        let manifest = run(&plan, &dest, ImportMode::PrefsOnly).unwrap();
        assert!(dest.join("settings.json").exists());
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.join("settings.json")).unwrap()).unwrap();
        assert!(settings.get("env").is_none());
        assert_eq!(fs::read(dest.join(".credentials.json")).unwrap(), creds);
        assert!(manifest.paths.contains(&"settings.json".into()));
        assert!(manifest.paths.contains(&"history.jsonl".into()));
        assert!(manifest.paths.contains(&".claude.json".into()));
    }

    #[test]
    fn rejects_symlink_escape_from_source_root() {
        let dir = ScratchDir::new("claude-local-import-symlink");
        let source_home = dir.path.join("src");
        let outside = dir.path.join("outside");
        fs::create_dir_all(&source_home).unwrap();
        fs::write(&outside, "secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, source_home.join("settings.json")).unwrap();
            let source_json = dir.path.join("missing.json");
            let err = plan(
                &source_home,
                &source_json,
                &ImportOptions {
                    prefs: true,
                    project_trust: false,
                    prompt_history: false,
                    ..ImportOptions::comfort_defaults()
                },
            );
            assert!(err.is_err());
        }
    }

    #[test]
    fn estimate_toggle_bytes_reports_missing_and_present_sizes_per_toggle() {
        let dir = ScratchDir::new("claude-local-import-estimate");
        let home = dir.path.join("home");
        let json = dir.path.join("claude.json");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("settings.json"), "12345").unwrap();
        fs::write(&json, "1234567890").unwrap();
        // history.jsonl intentionally left missing.

        let options = ImportOptions {
            prefs: true,
            prompt_history: true,
            project_trust: false,
            ..ImportOptions::comfort_defaults()
        };
        let estimates = estimate_toggle_bytes(&home, &json, &options);

        assert_eq!(
            estimates,
            vec![
                ("prefs".to_string(), Some(5)),
                ("prompt_history".to_string(), None),
            ]
        );
    }

    #[test]
    fn replace_mode_deletes_stale_paths_but_keeps_credentials() {
        let dir = ScratchDir::new("claude-local-import-replace");
        let source_home = dir.path.join("src");
        let source_json = dir.path.join("src.json");
        let dest = dir.path.join("dest");
        fs::create_dir_all(&source_home).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(source_home.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        fs::write(&source_json, r#"{"hasCompletedOnboarding":true}"#).unwrap();
        fs::write(dest.join(".credentials.json"), b"secret").unwrap();
        fs::write(dest.join("stale-import-artifact.txt"), b"old").unwrap();
        persist_manifest_state(
            &dest,
            &[
                "stale-import-artifact.txt".to_string(),
                "settings.json".to_string(),
            ],
        )
        .unwrap();

        let options = ImportOptions {
            prefs: true,
            project_trust: false,
            prompt_history: false,
            ..ImportOptions::comfort_defaults()
        };
        let plan = plan(&source_home, &source_json, &options).unwrap();
        run(&plan, &dest, ImportMode::Replace).unwrap();

        assert!(!dest.join("stale-import-artifact.txt").exists());
        assert!(dest.join("settings.json").exists());
        assert_eq!(fs::read(dest.join(".credentials.json")).unwrap(), b"secret");
    }
}
