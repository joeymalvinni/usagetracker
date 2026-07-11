use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};
use usage_core::ProviderId;

use crate::{
    config::{Config, ProviderConfig},
    polling::RefreshCoordinator,
    providers::paths::expand_home_path,
};

const CHANGE_DEBOUNCE: Duration = Duration::from_secs(30);
const LOCAL_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(30);
const CLAUDE_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);
const WATCH_EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Debug, Default)]
pub struct LocalLogConfig {
    codex: ProviderConfig,
    claude: ProviderConfig,
}

impl LocalLogConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            codex: config.providers.get("codex").cloned().unwrap_or_default(),
            claude: config.providers.get("claude").cloned().unwrap_or_default(),
        }
    }
}

pub fn spawn_change_log_loop(
    refresh: Arc<RefreshCoordinator>,
    mut configs: tokio::sync::watch::Receiver<LocalLogConfig>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut targets = local_log_targets(&configs.borrow().clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(WATCH_EVENT_QUEUE_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = overflowed.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |event| {
                if tx.try_send(event).is_err() {
                    callback_overflowed.store(true, Ordering::Release);
                }
            },
            NotifyConfig::default(),
        ) {
            Ok(watcher) => watcher,
            Err(err) => {
                warn!(error = %err, "failed to start local message log watcher");
                return;
            }
        };

        let mut watch_state = LocalLogWatchState::default();
        let _ = watch_state.sync(&mut watcher, &targets);
        info!(
            watched_roots = watch_state.watched_roots.len(),
            targets = ?target_roots(&targets),
            "local message log watcher started"
        );

        let mut pending = BTreeSet::<String>::new();
        let mut refresh_at = None;
        let mut last_refresh = None;
        loop {
            let deadline = refresh_at;
            tokio::select! {
                changed = configs.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    targets = local_log_targets(&configs.borrow().clone());
                    watch_state.reset(&mut watcher);
                    let newly_available = watch_state.sync(&mut watcher, &targets);
                    let active = targets
                        .iter()
                        .map(|target| target.provider_id)
                        .collect::<BTreeSet<_>>();
                    pending.retain(|provider| active.contains(provider.as_str()));
                    pending.extend(newly_available);
                    refresh_at = (!pending.is_empty()).then(|| local_refresh_deadline(
                        tokio::time::Instant::now(),
                        last_refresh,
                        &pending,
                    ));
                    info!(
                        watched_roots = watch_state.watched_roots.len(),
                        targets = ?target_roots(&targets),
                        "local message log watcher targets updated"
                    );
                }
                event = rx.recv() => {
                    let Some(event) = event else { return };
                    if handle_watch_event(
                        event,
                        &mut watch_state,
                        &mut watcher,
                        &targets,
                        &mut pending,
                    ) {
                        refresh_at = Some(local_refresh_deadline(
                            tokio::time::Instant::now(),
                            last_refresh,
                            &pending,
                        ));
                    }
                }
                _ = wait_for_deadline(deadline) => {
                    if overflowed.swap(false, Ordering::AcqRel) {
                        pending.extend(targets.iter().map(|target| target.provider_id.to_string()));
                        warn!(
                            queue_capacity = WATCH_EVENT_QUEUE_CAPACITY,
                            "local message log watcher queue overflowed; refreshing all local providers"
                        );
                    }
                    refresh_local_usage(&refresh, std::mem::take(&mut pending)).await;
                    last_refresh = Some(tokio::time::Instant::now());
                    refresh_at = None;
                }
            }
        }
    })
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

impl LocalLogWatchState {
    fn reset(&mut self, watcher: &mut RecommendedWatcher) {
        for path in std::mem::take(&mut self.watched_paths) {
            if let Err(err) = watcher.unwatch(&path) {
                debug!(path = %path.display(), error = %err, "failed to remove obsolete local log watch");
            }
        }
        self.watched_roots.clear();
    }
}

fn handle_watch_event(
    event: notify::Result<Event>,
    watch_state: &mut LocalLogWatchState,
    watcher: &mut RecommendedWatcher,
    targets: &[LocalLogTarget],
    pending: &mut BTreeSet<String>,
) -> bool {
    match event {
        Ok(event) => {
            let newly_available = watch_state.sync(watcher, targets);
            let should_refresh = !newly_available.is_empty();
            pending.extend(newly_available);
            let provider_id = provider_id_for_event(targets, &event);
            if let Some(provider_id) = provider_id {
                pending.insert(provider_id.to_string());
            }
            should_refresh || provider_id.is_some()
        }
        Err(err) => {
            warn!(error = %err, "local message log watcher error");
            false
        }
    }
}

fn local_refresh_deadline(
    now: tokio::time::Instant,
    last_refresh: Option<tokio::time::Instant>,
    pending: &BTreeSet<String>,
) -> tokio::time::Instant {
    let debounced = now + CHANGE_DEBOUNCE;
    let minimum_interval = if pending.contains("claude") {
        CLAUDE_REFRESH_MIN_INTERVAL
    } else {
        LOCAL_REFRESH_MIN_INTERVAL
    };
    let rate_limited = last_refresh
        .map(|last| last + minimum_interval)
        .unwrap_or(now);
    debounced.max(rate_limited)
}

#[derive(Debug)]
struct LocalLogTarget {
    provider_id: &'static str,
    roots: Vec<PathBuf>,
}

#[derive(Default)]
struct LocalLogWatchState {
    watched_paths: BTreeSet<PathBuf>,
    watched_roots: BTreeSet<PathBuf>,
}

impl LocalLogWatchState {
    fn sync(
        &mut self,
        watcher: &mut RecommendedWatcher,
        targets: &[LocalLogTarget],
    ) -> BTreeSet<String> {
        let mut available_providers = BTreeSet::new();
        for target in targets {
            for root in &target.roots {
                if root.is_dir() {
                    if self.watch_root(watcher, target.provider_id, root) {
                        self.watched_roots.insert(root.clone());
                        available_providers.insert(target.provider_id.to_string());
                    }
                    continue;
                }

                if self.watched_roots.contains(root) {
                    continue;
                }

                match nearest_existing_parent(root) {
                    Some(parent) => {
                        let _ = self.watch_path(
                            watcher,
                            target.provider_id,
                            &parent,
                            RecursiveMode::NonRecursive,
                        );
                    }
                    None => info!(
                        provider_id = target.provider_id,
                        root = %root.display(),
                        "local message log root and parent directories do not exist; not watching it"
                    ),
                }
            }
        }
        available_providers
    }

    fn watch_root(
        &mut self,
        watcher: &mut RecommendedWatcher,
        provider_id: &str,
        root: &Path,
    ) -> bool {
        if self.watched_roots.contains(root) {
            return false;
        }

        match watcher.watch(root, RecursiveMode::Recursive) {
            Ok(()) => {
                self.watched_paths.insert(root.to_path_buf());
                true
            }
            Err(err) => {
                warn!(
                    provider_id,
                    root = %root.display(),
                    error = %err,
                    "failed to watch local message log root"
                );
                false
            }
        }
    }

    fn watch_path(
        &mut self,
        watcher: &mut RecommendedWatcher,
        provider_id: &str,
        path: &Path,
        mode: RecursiveMode,
    ) -> bool {
        if self.watched_paths.contains(path) {
            return false;
        }

        match watcher.watch(path, mode) {
            Ok(()) => {
                self.watched_paths.insert(path.to_path_buf());
                true
            }
            Err(err) => {
                warn!(
                    provider_id,
                    path = %path.display(),
                    error = %err,
                    "failed to watch local message log path"
                );
                false
            }
        }
    }
}

fn nearest_existing_parent(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .skip(1)
        .find(|path| path.is_dir())
        .map(Path::to_path_buf)
}

fn provider_id_for_event<'a>(targets: &'a [LocalLogTarget], event: &Event) -> Option<&'a str> {
    let paths = jsonl_paths(&event.paths);
    if paths.is_empty() {
        return None;
    }

    let provider_id = provider_id_for_paths(targets, &paths);
    debug!(
        provider_id = provider_id.unwrap_or("unknown"),
        kind = ?event.kind,
        paths = ?display_paths(&paths),
        "local message logs changed; scheduling usage refresh"
    );
    provider_id
}

async fn refresh_local_usage(refresh: &RefreshCoordinator, pending: BTreeSet<String>) {
    if pending.is_empty() {
        return;
    }

    let providers = pending.into_iter().map(ProviderId::new).collect::<Vec<_>>();
    let report = refresh.refresh(Some(&providers)).await;
    info!(
        provider_filter = ?providers.iter().map(ProviderId::as_str).collect::<Vec<_>>(),
        results = report.provider_results.len(),
        "local message log refresh completed"
    );
}

fn local_log_targets(config: &LocalLogConfig) -> Vec<LocalLogTarget> {
    let mut targets = Vec::new();
    if config.codex.enabled {
        targets.push(LocalLogTarget {
            provider_id: "codex",
            roots: codex_session_roots(&config.codex),
        });
    }
    if config.claude.enabled {
        targets.push(LocalLogTarget {
            provider_id: "claude",
            roots: claude_project_roots(&config.claude),
        });
    }
    targets
}

fn codex_session_roots(config: &ProviderConfig) -> Vec<PathBuf> {
    let codex_home = match std::env::var("CODEX_HOME") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => match dirs::home_dir() {
            Some(home) => home.join(".codex"),
            None => return Vec::new(),
        },
    };
    let mut roots = vec![codex_home.join("sessions")];
    roots.extend(
        config
            .profiles
            .iter()
            .filter(|profile| profile.enabled && !profile.deleted)
            .filter_map(|profile| profile.codex_home.as_deref())
            .map(expand_home_path)
            .map(|home| home.join("sessions")),
    );
    roots.sort();
    roots.dedup();
    roots
}

fn claude_project_roots(config: &ProviderConfig) -> Vec<PathBuf> {
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
        usage_core::default_app_dir().map(|root| root.join("profiles").join("claude"));
    if let Some(root) = managed_root.as_ref() {
        // Watch the common parent so profiles created after daemon startup are picked up
        // without rebuilding the filesystem watcher.
        roots.push(root.clone());
    }
    for profile in config
        .profiles
        .iter()
        .filter(|profile| profile.enabled && !profile.deleted)
    {
        let configured = if profile.project_roots.is_empty() {
            profile
                .claude_config_dir
                .as_ref()
                .map(|root| vec![expand_home_path(root.clone()).join("projects")])
                .unwrap_or_default()
        } else {
            profile
                .project_roots
                .iter()
                .cloned()
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
    roots
}

fn jsonl_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .cloned()
        .collect()
}

fn provider_id_for_paths<'a>(targets: &'a [LocalLogTarget], paths: &[PathBuf]) -> Option<&'a str> {
    targets.iter().find_map(|target| {
        paths
            .iter()
            .any(|path| target.roots.iter().any(|root| path.starts_with(root)))
            .then_some(target.provider_id)
    })
}

fn target_roots(targets: &[LocalLogTarget]) -> Vec<String> {
    targets
        .iter()
        .flat_map(|target| display_paths(&target.roots))
        .collect()
}

fn display_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn keeps_only_jsonl_paths() {
        let paths = jsonl_paths(&[
            PathBuf::from("/tmp/session.jsonl"),
            PathBuf::from("/tmp/session.json"),
            PathBuf::from("/tmp/nested"),
        ]);

        assert_eq!(paths, vec![PathBuf::from("/tmp/session.jsonl")]);
    }

    #[test]
    fn resolves_provider_from_watched_roots() {
        let targets = vec![
            LocalLogTarget {
                provider_id: "codex",
                roots: vec![PathBuf::from("/home/me/.codex/sessions")],
            },
            LocalLogTarget {
                provider_id: "claude",
                roots: vec![PathBuf::from("/home/me/.claude/projects")],
            },
        ];

        let provider = provider_id_for_paths(
            &targets,
            &[PathBuf::from(
                "/home/me/.claude/projects/project/session.jsonl",
            )],
        );

        assert_eq!(provider, Some("claude"));
    }

    #[test]
    fn finds_nearest_existing_parent_for_missing_root() {
        let base =
            std::env::temp_dir().join(format!("usage-local-logs-test-{}", uuid::Uuid::new_v4()));
        let existing = base.join(".codex");
        let missing_root = existing.join("sessions");
        fs::create_dir_all(&existing).unwrap();

        assert_eq!(nearest_existing_parent(&missing_root), Some(existing));

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn refresh_deadline_debounces_and_rate_limits() {
        let now = tokio::time::Instant::now();
        let codex = BTreeSet::from(["codex".to_string()]);
        let claude = BTreeSet::from(["claude".to_string()]);
        assert_eq!(
            local_refresh_deadline(now, None, &codex),
            now + CHANGE_DEBOUNCE
        );

        let recent = now - Duration::from_secs(5);
        assert_eq!(
            local_refresh_deadline(now, Some(recent), &codex),
            now + CHANGE_DEBOUNCE
        );
        assert_eq!(
            local_refresh_deadline(now, Some(recent), &claude),
            recent + CLAUDE_REFRESH_MIN_INTERVAL
        );
    }

    #[test]
    fn watches_managed_and_manual_claude_profile_roots_separately() {
        let config = ProviderConfig {
            enabled: true,
            profiles: vec![
                crate::config::ProviderProfileConfig {
                    id: Some("managed".to_string()),
                    claude_config_dir: usage_core::default_app_dir()
                        .map(|root| root.join("profiles/claude/managed")),
                    ..crate::config::ProviderProfileConfig::default()
                },
                crate::config::ProviderProfileConfig {
                    id: Some("manual".to_string()),
                    project_roots: vec![PathBuf::from("/tmp/manual-claude/projects")],
                    ..crate::config::ProviderProfileConfig::default()
                },
                crate::config::ProviderProfileConfig {
                    id: Some("disabled".to_string()),
                    enabled: false,
                    project_roots: vec![PathBuf::from("/tmp/disabled-claude/projects")],
                    ..crate::config::ProviderProfileConfig::default()
                },
            ],
            ..ProviderConfig::default()
        };

        let roots = claude_project_roots(&config);

        assert!(roots.contains(&PathBuf::from("/tmp/manual-claude/projects")));
        assert!(!roots.contains(&PathBuf::from("/tmp/disabled-claude/projects")));
        if let Some(managed) =
            usage_core::default_app_dir().map(|root| root.join("profiles").join("claude"))
        {
            assert!(roots.contains(&managed));
            assert!(!roots.contains(&managed.join("managed/projects")));
        }
    }
}
