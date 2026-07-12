use std::{
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context};
use usage_core::default_app_dir;
use uuid::Uuid;

const PROFILES_DIR: &str = "profiles";
const QUARANTINE_DIR: &str = ".quarantine";
const CODEX_PROVIDER_ID: &str = "codex";
const CLAUDE_PROVIDER_ID: &str = "claude";
const GROK_PROVIDER_ID: &str = "grok";

/// Returns the only directory that may represent a managed provider profile.
///
/// The path is derived from a trusted application root and a single validated
/// profile-id component. Configured paths are deliberately not accepted here.
pub(crate) fn profile_home(provider_id: &str, profile_id: &str) -> anyhow::Result<PathBuf> {
    let app_root =
        default_app_dir().context("failed to resolve the usage tracker application directory")?;
    profile_home_in(&app_root, provider_id, profile_id)
}

pub(crate) fn is_managed_profile(path: &Path, provider_id: &str) -> bool {
    let Some(app_root) = default_app_dir() else {
        return false;
    };
    let Ok(root) = provider_root(&app_root, provider_id) else {
        return false;
    };
    path.parent() == Some(root.as_path())
        && path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|id| validate_profile_id(id).is_ok())
}

/// Validates a configured profile path without ever using it as the deletion
/// target. A valid configuration must name the exact path derived from the
/// provider and profile id. Existing paths are canonicalized and symlinks are
/// rejected so an alias cannot escape the managed root.
pub(crate) fn deletion_candidate(
    provider_id: &str,
    profile_id: &str,
    configured_path: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(configured_path) = configured_path else {
        return Ok(None);
    };
    let app_root =
        default_app_dir().context("failed to resolve the usage tracker application directory")?;
    deletion_candidate_in(&app_root, provider_id, profile_id, configured_path)
}

/// Atomically detaches a validated profile from its public name before doing
/// recursive removal. This both narrows the race window and makes a crash
/// during cleanup recoverable as an orphaned quarantine entry.
pub(crate) fn quarantine_and_remove(candidate: &Path) -> anyhow::Result<()> {
    if !candidate.exists() {
        return Ok(());
    }
    let provider_root = candidate
        .parent()
        .context("managed profile candidate has no provider root")?;
    validate_existing_candidate(provider_root, candidate)?;

    let quarantine_root = provider_root.join(QUARANTINE_DIR);
    fs::create_dir_all(&quarantine_root).with_context(|| {
        format!(
            "failed to create managed profile quarantine {}",
            quarantine_root.display()
        )
    })?;
    let canonical_provider_root = fs::canonicalize(provider_root)?;
    let canonical_quarantine_root = fs::canonicalize(&quarantine_root)?;
    if canonical_quarantine_root.parent() != Some(canonical_provider_root.as_path()) {
        bail!(
            "managed profile quarantine escaped provider root: {}",
            quarantine_root.display()
        );
    }

    let quarantined = quarantine_root.join(Uuid::new_v4().to_string());
    fs::rename(candidate, &quarantined).with_context(|| {
        format!(
            "failed to quarantine managed profile {}",
            candidate.display()
        )
    })?;
    fs::remove_dir_all(&quarantined).with_context(|| {
        format!(
            "failed to remove quarantined managed profile {}",
            quarantined.display()
        )
    })
}

fn deletion_candidate_in(
    app_root: &Path,
    provider_id: &str,
    profile_id: &str,
    configured_path: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    reject_ambiguous_components(configured_path)?;
    let candidate = profile_home_in(app_root, provider_id, profile_id)?;
    if configured_path != candidate {
        let root = provider_root(app_root, provider_id)?;
        if configured_path.starts_with(&root) {
            bail!("configured profile path does not match managed profile id '{profile_id}'");
        }
        return Ok(None);
    }
    if !candidate.exists() {
        return Ok(Some(candidate));
    }
    let root = candidate
        .parent()
        .context("managed profile candidate has no provider root")?;
    validate_existing_candidate(root, &candidate)?;
    Ok(Some(candidate))
}

fn validate_existing_candidate(root: &Path, candidate: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(candidate)
        .with_context(|| format!("failed to inspect managed profile {}", candidate.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "managed profile path may not be a symlink: {}",
            candidate.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "managed profile path is not a directory: {}",
            candidate.display()
        );
    }
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve managed root {}", root.display()))?;
    let canonical_candidate = fs::canonicalize(candidate)
        .with_context(|| format!("failed to resolve managed profile {}", candidate.display()))?;
    if canonical_candidate == canonical_root
        || canonical_candidate.parent() != Some(canonical_root.as_path())
    {
        bail!(
            "managed profile resolved outside its provider root: {}",
            candidate.display()
        );
    }
    Ok(())
}

fn profile_home_in(
    app_root: &Path,
    provider_id: &str,
    profile_id: &str,
) -> anyhow::Result<PathBuf> {
    validate_profile_id(profile_id)?;
    Ok(provider_root(app_root, provider_id)?.join(profile_id))
}

fn provider_root(app_root: &Path, provider_id: &str) -> anyhow::Result<PathBuf> {
    if !matches!(
        provider_id,
        CODEX_PROVIDER_ID | CLAUDE_PROVIDER_ID | GROK_PROVIDER_ID
    ) {
        bail!("provider '{provider_id}' does not have managed profiles");
    }
    Ok(app_root.join(PROFILES_DIR).join(provider_id))
}

fn validate_profile_id(profile_id: &str) -> anyhow::Result<()> {
    if profile_id.is_empty() || profile_id == QUARANTINE_DIR {
        bail!("managed profile id is empty or reserved");
    }
    let mut components = Path::new(profile_id).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(value)), None) if value == OsStr::new(profile_id) => Ok(()),
        _ => bail!("managed profile id must be one ordinary path component"),
    }
}

fn reject_ambiguous_components(path: &Path) -> anyhow::Result<()> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        bail!("configured managed profile path contains ambiguous components");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("managed-profile-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn derives_profile_path_from_one_validated_component() {
        let root = root();
        assert_eq!(
            profile_home_in(&root, CODEX_PROVIDER_ID, "work").unwrap(),
            root.join("profiles/codex/work")
        );
        assert_eq!(
            profile_home_in(&root, GROK_PROVIDER_ID, "work").unwrap(),
            root.join("profiles/grok/work")
        );
        for invalid in [
            "",
            ".",
            "..",
            "a/b",
            "a/../b",
            "/tmp/outside",
            ".quarantine",
        ] {
            assert!(profile_home_in(&root, CODEX_PROVIDER_ID, invalid).is_err());
        }
    }

    #[test]
    fn rejects_external_root_and_parent_traversal_configurations() {
        let root = root();
        let expected = profile_home_in(&root, CODEX_PROVIDER_ID, "work").unwrap();
        assert!(deletion_candidate_in(
            &root,
            CODEX_PROVIDER_ID,
            "work",
            &root.join("profiles/codex/../../../outside")
        )
        .is_err());
        assert_eq!(
            deletion_candidate_in(&root, CODEX_PROVIDER_ID, "work", &root.join("outside")).unwrap(),
            None
        );
        assert_eq!(
            deletion_candidate_in(&root, CODEX_PROVIDER_ID, "work", &expected).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn rejects_managed_root_and_external_symlink() {
        let root = root();
        let provider_root = root.join("profiles/codex");
        let outside = root.join("outside");
        fs::create_dir_all(&provider_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let candidate = provider_root.join("work");
        std::os::unix::fs::symlink(&outside, &candidate).unwrap();

        assert!(deletion_candidate_in(&root, CODEX_PROVIDER_ID, "work", &candidate).is_err());
        assert!(profile_home_in(&root, CODEX_PROVIDER_ID, "").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn quarantine_detaches_then_removes_only_candidate() {
        let root = root();
        let provider_root = root.join("profiles/claude");
        let candidate = provider_root.join("work");
        let sibling = provider_root.join("personal");
        fs::create_dir_all(candidate.join("nested")).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        quarantine_and_remove(&candidate).unwrap();

        assert!(!candidate.exists());
        assert!(sibling.exists());
        assert!(provider_root.join(QUARANTINE_DIR).exists());
        let _ = fs::remove_dir_all(root);
    }
}
