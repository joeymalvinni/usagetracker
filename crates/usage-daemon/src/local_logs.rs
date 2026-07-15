use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use usage_core::ProviderId;

use crate::{
    config::Config,
    polling::RefreshCoordinator,
    runtime::{provider_adapter::LocalUsagePathMatcher, provider_registry},
};

const WATCH_EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Debug, Default)]
pub struct LocalLogConfig {
    targets: Vec<LocalLogTarget>,
}

impl LocalLogConfig {
    pub fn from_config(config: &Config) -> Self {
        let mut targets = Vec::new();
        for descriptor in provider_registry::descriptors() {
            let Some(provider_config) = config.providers.get(descriptor.id.as_str()) else {
                continue;
            };
            if !provider_config.enabled {
                continue;
            }
            let Some(adapter) = provider_registry::find(descriptor.id.as_str()) else {
                continue;
            };
            match adapter.local_usage_watch(provider_config) {
                Ok(Some(watch)) => match watch.validate() {
                    Ok(()) => targets.push(LocalLogTarget {
                        provider_id: descriptor.id.as_str().to_string(),
                        roots: watch.roots,
                        matchers: watch.matchers,
                        debounce: watch.debounce,
                        maximum_latency: watch.maximum_latency,
                        minimum_refresh_interval: watch.minimum_refresh_interval,
                    }),
                    Err(error) => warn!(
                        provider_id = descriptor.id.as_str(),
                        %error,
                        "ignoring invalid provider-owned local usage watch"
                    ),
                },
                Ok(None) => {}
                Err(error) => warn!(
                    provider_id = descriptor.id.as_str(),
                    %error,
                    "failed to derive provider-owned local usage watch roots"
                ),
            }
        }
        Self { targets }
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
        let overflow_wakeup = Arc::new(Notify::new());
        let callback_overflow_wakeup = overflow_wakeup.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |event| {
                if tx.try_send(event).is_err() {
                    signal_queue_overflow(&callback_overflowed, &callback_overflow_wakeup);
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

        let mut pending = BTreeMap::<String, PendingRefresh>::new();
        let mut last_refresh = BTreeMap::<String, tokio::time::Instant>::new();
        loop {
            let deadline = pending.values().map(|pending| pending.deadline).min();
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
                        .map(|target| target.provider_id.clone())
                        .collect::<BTreeSet<_>>();
                    pending.retain(|provider, _| active.contains(provider));
                    last_refresh.retain(|provider, _| active.contains(provider));
                    schedule_providers(
                        tokio::time::Instant::now(),
                        newly_available,
                        &targets,
                        &last_refresh,
                        &mut pending,
                    );
                    info!(
                        watched_roots = watch_state.watched_roots.len(),
                        targets = ?target_roots(&targets),
                        "local message log watcher targets updated"
                    );
                }
                event = rx.recv() => {
                    let Some(event) = event else { return };
                    let changed = handle_watch_event(
                        event,
                        &mut watch_state,
                        &mut watcher,
                        &targets,
                    );
                    schedule_providers(
                        tokio::time::Instant::now(),
                        changed,
                        &targets,
                        &last_refresh,
                        &mut pending,
                    );
                }
                _ = overflow_wakeup.notified() => {
                    if overflowed.swap(false, Ordering::AcqRel) {
                        schedule_providers(
                            tokio::time::Instant::now(),
                            targets.iter().map(|target| target.provider_id.clone()).collect(),
                            &targets,
                            &last_refresh,
                            &mut pending,
                        );
                        warn!(
                            queue_capacity = WATCH_EVENT_QUEUE_CAPACITY,
                            "local message log watcher queue overflowed; reconciling all local providers"
                        );
                    }
                }
                _ = wait_for_deadline(deadline) => {
                    let now = tokio::time::Instant::now();
                    let due = pending
                        .iter()
                        .filter(|(_, pending)| pending.deadline <= now)
                        .map(|(provider, _)| provider.clone())
                        .collect::<BTreeSet<_>>();
                    for provider in &due {
                        pending.remove(provider);
                        last_refresh.insert(provider.clone(), now);
                    }
                    if !due.is_empty() {
                        let refresh = refresh.clone();
                        tokio::spawn(async move {
                            refresh_local_usage(&refresh, due).await;
                        });
                    }
                }
            }
        }
    })
}

fn signal_queue_overflow(overflowed: &AtomicBool, wakeup: &Notify) {
    overflowed.store(true, Ordering::Release);
    wakeup.notify_one();
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
) -> BTreeSet<String> {
    match event {
        Ok(event) => {
            let mut changed = watch_state.sync(watcher, targets);
            changed.extend(provider_ids_for_event(targets, &event));
            changed
        }
        Err(err) => {
            warn!(error = %err, "local message log watcher error");
            BTreeSet::new()
        }
    }
}

fn schedule_providers(
    now: tokio::time::Instant,
    providers: BTreeSet<String>,
    targets: &[LocalLogTarget],
    last_refresh: &BTreeMap<String, tokio::time::Instant>,
    pending: &mut BTreeMap<String, PendingRefresh>,
) {
    for provider in providers {
        let Some(target) = targets.iter().find(|target| target.provider_id == provider) else {
            continue;
        };
        let first_change = pending
            .get(&provider)
            .map(|pending| pending.first_change)
            .unwrap_or(now);
        let debounced = now + target.debounce;
        let bounded = first_change + target.maximum_latency;
        let rate_limited = last_refresh
            .get(&provider)
            .map(|last| *last + target.minimum_refresh_interval)
            .unwrap_or(now);
        pending.insert(
            provider,
            PendingRefresh {
                first_change,
                deadline: debounced.min(bounded).max(rate_limited),
            },
        );
    }
}

#[derive(Clone, Debug)]
struct LocalLogTarget {
    provider_id: String,
    roots: Vec<PathBuf>,
    matchers: Vec<LocalUsagePathMatcher>,
    debounce: Duration,
    maximum_latency: Duration,
    minimum_refresh_interval: Duration,
}

#[derive(Clone, Copy, Debug)]
struct PendingRefresh {
    first_change: tokio::time::Instant,
    deadline: tokio::time::Instant,
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
                    if self.watch_root(watcher, &target.provider_id, root) {
                        self.watched_roots.insert(root.clone());
                        available_providers.insert(target.provider_id.clone());
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
                            &target.provider_id,
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

fn provider_ids_for_event(targets: &[LocalLogTarget], event: &Event) -> BTreeSet<String> {
    let provider_ids = provider_ids_for_paths(targets, &event.paths);
    debug!(
        provider_ids = ?provider_ids,
        kind = ?event.kind,
        paths = ?display_paths(&event.paths),
        "local message logs changed; scheduling usage refresh"
    );
    provider_ids
}

async fn refresh_local_usage(refresh: &RefreshCoordinator, pending: BTreeSet<String>) {
    if pending.is_empty() {
        return;
    }

    let providers = pending.into_iter().map(ProviderId::new).collect::<Vec<_>>();
    let datasets = refresh.refresh_local(&providers).await;
    info!(
        provider_filter = ?providers.iter().map(ProviderId::as_str).collect::<Vec<_>>(),
        datasets,
        "local message log refresh completed"
    );
}

fn local_log_targets(config: &LocalLogConfig) -> Vec<LocalLogTarget> {
    config.targets.clone()
}

fn provider_ids_for_paths(targets: &[LocalLogTarget], paths: &[PathBuf]) -> BTreeSet<String> {
    targets
        .iter()
        .filter(|target| {
            paths.iter().any(|path| {
                target.roots.iter().any(|root| path.starts_with(root))
                    && target.matchers.iter().any(|matcher| matcher.matches(path))
            })
        })
        .map(|target| target.provider_id.clone())
        .collect()
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

    fn target(
        provider_id: &str,
        root: &str,
        matcher: LocalUsagePathMatcher,
        minimum_refresh_interval: Duration,
    ) -> LocalLogTarget {
        LocalLogTarget {
            provider_id: provider_id.to_string(),
            roots: vec![PathBuf::from(root)],
            matchers: vec![matcher],
            debounce: Duration::from_secs(30),
            maximum_latency: Duration::from_secs(60),
            minimum_refresh_interval,
        }
    }

    #[test]
    fn resolves_every_provider_with_a_matching_root_and_file_pattern() {
        let targets = vec![
            target(
                "alpha",
                "/home/me/.shared/sessions",
                LocalUsagePathMatcher::extension("jsonl"),
                Duration::from_secs(30),
            ),
            target(
                "beta",
                "/home/me/.shared",
                LocalUsagePathMatcher::file_name("session.jsonl"),
                Duration::from_secs(60),
            ),
            target(
                "database",
                "/home/me/.local/share/opencode",
                LocalUsagePathMatcher::suffix(".db-wal"),
                Duration::from_secs(60),
            ),
        ];

        let providers = provider_ids_for_paths(
            &targets,
            &[PathBuf::from(
                "/home/me/.shared/sessions/project/session.jsonl",
            )],
        );

        assert_eq!(
            providers,
            BTreeSet::from(["alpha".to_string(), "beta".to_string()])
        );
        assert!(provider_ids_for_paths(
            &targets,
            &[PathBuf::from(
                "/home/me/.local/share/opencode/opencode.db-wal"
            )]
        )
        .contains("database"));
        assert!(provider_ids_for_paths(
            &targets,
            &[PathBuf::from(
                "/home/me/.shared/sessions/project/session.json"
            )]
        )
        .is_empty());
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
    fn scheduling_is_independent_and_has_bounded_trailing_debounce() {
        let now = tokio::time::Instant::now();
        let targets = vec![
            target(
                "fast",
                "/fast",
                LocalUsagePathMatcher::extension("jsonl"),
                Duration::from_secs(30),
            ),
            target(
                "slow",
                "/slow",
                LocalUsagePathMatcher::extension("jsonl"),
                Duration::from_secs(90),
            ),
        ];
        let mut pending = BTreeMap::new();
        let last_refresh = BTreeMap::from([("slow".to_string(), now - Duration::from_secs(5))]);

        schedule_providers(
            now,
            BTreeSet::from(["fast".to_string(), "slow".to_string()]),
            &targets,
            &last_refresh,
            &mut pending,
        );
        assert_eq!(pending["fast"].deadline, now + Duration::from_secs(30));
        assert_eq!(pending["slow"].deadline, now + Duration::from_secs(85));

        schedule_providers(
            now + Duration::from_secs(50),
            BTreeSet::from(["fast".to_string()]),
            &targets,
            &last_refresh,
            &mut pending,
        );
        assert_eq!(pending["fast"].deadline, now + Duration::from_secs(60));
        assert_eq!(pending["slow"].deadline, now + Duration::from_secs(85));
    }

    #[tokio::test]
    async fn queue_overflow_wakes_reconciliation_without_a_refresh_deadline() {
        let overflowed = AtomicBool::new(false);
        let wakeup = Notify::new();
        let notified = wakeup.notified();

        signal_queue_overflow(&overflowed, &wakeup);

        tokio::time::timeout(Duration::from_millis(10), notified)
            .await
            .expect("overflow should wake the watcher loop");
        assert!(overflowed.swap(false, Ordering::AcqRel));
    }
}
