use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::Context;
use chrono::NaiveDate;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheStatus {
    Hit,
    Throttled,
    Refreshed,
}

impl CacheStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Throttled => "throttled",
            Self::Refreshed => "refreshed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileFingerprint {
    size: u64,
    modified_ns: u128,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FileSetFingerprint {
    files: usize,
    total_size: u64,
    latest_modified_ns: u128,
    digest: u64,
}

#[derive(Clone, Debug)]
struct FileSnapshot {
    path: PathBuf,
    fingerprint: FileFingerprint,
}

#[derive(Debug)]
pub(crate) struct CachedFile<S> {
    fingerprint: FileFingerprint,
    pub(crate) summary: Arc<S>,
}

impl<S> Clone for CachedFile<S> {
    fn clone(&self) -> Self {
        Self {
            fingerprint: self.fingerprint,
            summary: Arc::clone(&self.summary),
        }
    }
}

impl<S> CachedFile<S> {
    pub(crate) fn summary(&self) -> &S {
        &self.summary
    }
}

#[derive(Debug)]
pub(crate) struct LocalFileCache<S, R> {
    roots: Vec<PathBuf>,
    revision: u64,
    fingerprint: FileSetFingerprint,
    pub(crate) files: BTreeMap<PathBuf, CachedFile<S>>,
    pub(crate) file_order: Vec<PathBuf>,
    report: R,
    report_date: NaiveDate,
    pub(crate) scanned_at: Instant,
}

impl<S, R: Clone> Clone for LocalFileCache<S, R> {
    fn clone(&self) -> Self {
        Self {
            roots: self.roots.clone(),
            revision: self.revision,
            fingerprint: self.fingerprint,
            files: self.files.clone(),
            file_order: self.file_order.clone(),
            report: self.report.clone(),
            report_date: self.report_date,
            scanned_at: self.scanned_at,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LocalFileScan<R> {
    pub(crate) report: R,
    pub(crate) cache_status: CacheStatus,
}

/// Scans a set of local provider files while reusing unchanged per-file
/// summaries. Provider code supplies only file parsing and report folding.
///
/// `revision` invalidates every parsed summary (for example when a pricing
/// catalog changes). `report_date` invalidates only the folded rolling report,
/// so crossing midnight does not re-read unchanged JSONL files.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_cached_files<S, R, Parse, Fold>(
    cache: Arc<Mutex<Option<LocalFileCache<S, R>>>>,
    mut roots: Vec<PathBuf>,
    extension: &str,
    revision: u64,
    min_interval: Duration,
    report_date: NaiveDate,
    parse_file: Parse,
    fold_report: Fold,
) -> anyhow::Result<LocalFileScan<R>>
where
    R: Clone,
    Parse: Fn(&Path) -> anyhow::Result<S>,
    Fold: Fn(&[PathBuf], &BTreeMap<PathBuf, CachedFile<S>>, NaiveDate) -> R,
{
    roots.sort();
    roots.dedup();
    // Snapshot cache state quickly, then release the mutex before walking the
    // filesystem, parsing JSONL, or folding reports. Concurrent account scans
    // may do redundant work, but they never form a queue behind one long scan.
    let mut cached = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("local file cache mutex poisoned"))?
        .clone();
    let baseline_scan = cached.as_ref().map(|value| value.scanned_at);

    if let Some(cached) = cached.as_mut() {
        if cached.roots == roots
            && cached.revision == revision
            && cached.scanned_at.elapsed() < min_interval
        {
            if cached.report_date != report_date {
                cached.report = fold_report(&cached.file_order, &cached.files, report_date);
                cached.report_date = report_date;
                replace_cache_if_unchanged(&cache, baseline_scan, cached.clone())?;
            }
            return Ok(LocalFileScan {
                report: cached.report.clone(),
                cache_status: CacheStatus::Throttled,
            });
        }
    }

    let snapshots = collect_file_snapshots(&roots, extension)?;
    let fingerprint = file_set_fingerprint(&snapshots);
    let same_scope = cached
        .as_ref()
        .is_some_and(|cached| cached.roots == roots && cached.revision == revision);

    if same_scope
        && cached
            .as_ref()
            .is_some_and(|cached| cached.fingerprint == fingerprint)
    {
        let cached = cached.as_mut().expect("same scope requires a cache");
        if cached.report_date != report_date {
            cached.report = fold_report(&cached.file_order, &cached.files, report_date);
            cached.report_date = report_date;
        }
        cached.scanned_at = Instant::now();
        replace_cache_if_unchanged(&cache, baseline_scan, cached.clone())?;
        return Ok(LocalFileScan {
            report: cached.report.clone(),
            cache_status: CacheStatus::Hit,
        });
    }

    let mut files = BTreeMap::new();
    for snapshot in &snapshots {
        let reusable = cached
            .as_ref()
            .filter(|cached| cached.revision == revision)
            .and_then(|cached| cached.files.get(&snapshot.path))
            .filter(|cached| cached.fingerprint == snapshot.fingerprint)
            .cloned();
        let file = match reusable {
            Some(file) => file,
            None => CachedFile {
                fingerprint: snapshot.fingerprint,
                summary: Arc::new(parse_file(&snapshot.path).with_context(|| {
                    format!(
                        "failed to scan local usage file {}",
                        snapshot.path.display()
                    )
                })?),
            },
        };
        files.insert(snapshot.path.clone(), file);
    }
    let file_order = snapshots
        .iter()
        .map(|snapshot| snapshot.path.clone())
        .collect::<Vec<_>>();
    let report = fold_report(&file_order, &files, report_date);
    let refreshed = LocalFileCache {
        roots,
        revision,
        fingerprint,
        files,
        file_order,
        report: report.clone(),
        report_date,
        scanned_at: Instant::now(),
    };
    replace_cache_if_unchanged(&cache, baseline_scan, refreshed)?;
    Ok(LocalFileScan {
        report,
        cache_status: CacheStatus::Refreshed,
    })
}

fn replace_cache_if_unchanged<S, R>(
    cache: &Mutex<Option<LocalFileCache<S, R>>>,
    baseline_scan: Option<Instant>,
    replacement: LocalFileCache<S, R>,
) -> anyhow::Result<()> {
    let mut current = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("local file cache mutex poisoned"))?;
    if current.as_ref().map(|value| value.scanned_at) == baseline_scan {
        *current = Some(replacement);
    }
    Ok(())
}

fn collect_file_snapshots(roots: &[PathBuf], extension: &str) -> anyhow::Result<Vec<FileSnapshot>> {
    let mut files = BTreeMap::<PathBuf, FileFingerprint>::new();
    let mut visited_directories = BTreeSet::new();
    let extension = OsStr::new(extension);
    for root in roots {
        collect_file_snapshots_from_path(root, extension, &mut visited_directories, &mut files)?;
    }
    Ok(files
        .into_iter()
        .map(|(path, fingerprint)| FileSnapshot { path, fingerprint })
        .collect())
}

fn collect_file_snapshots_from_path(
    path: &Path,
    extension: &OsStr,
    visited_directories: &mut BTreeSet<PathBuf>,
    files: &mut BTreeMap<PathBuf, FileFingerprint>,
) -> anyhow::Result<()> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited_directories.insert(canonical) {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let is_target = path.extension() == Some(extension);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if is_target => return Err(error.into()),
            Err(_) => continue,
        };
        if metadata.is_dir() {
            collect_file_snapshots_from_path(&path, extension, visited_directories, files)?;
        } else if is_target {
            files.insert(
                path,
                FileFingerprint {
                    size: metadata.len(),
                    modified_ns: metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default(),
                },
            );
        }
    }
    Ok(())
}

fn file_set_fingerprint(files: &[FileSnapshot]) -> FileSetFingerprint {
    let mut digest = DefaultHasher::new();
    let mut fingerprint = FileSetFingerprint {
        files: files.len(),
        ..Default::default()
    };
    for file in files {
        file.path.hash(&mut digest);
        file.fingerprint.size.hash(&mut digest);
        file.fingerprint.modified_ns.hash(&mut digest);
        fingerprint.total_size = fingerprint.total_size.saturating_add(file.fingerprint.size);
        fingerprint.latest_modified_ns = fingerprint
            .latest_modified_ns
            .max(file.fingerprint.modified_ns);
    }
    fingerprint.digest = digest.finish();
    fingerprint
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use chrono::Days;

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("local-file-cache-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_only_changed_files_and_refolds_at_midnight() {
        let dir = TestDir::new();
        let first_path = dir.0.join("first.jsonl");
        let second_path = dir.0.join("second.jsonl");
        std::fs::write(&first_path, "one").unwrap();
        std::fs::write(&second_path, "two").unwrap();
        let cache = Arc::new(Mutex::new(None));
        let parses = Arc::new(AtomicUsize::new(0));
        let folds = Arc::new(AtomicUsize::new(0));
        let today = NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();

        let scan = |date, interval| {
            let parses = Arc::clone(&parses);
            let folds = Arc::clone(&folds);
            scan_cached_files(
                Arc::clone(&cache),
                vec![dir.0.clone()],
                "jsonl",
                1,
                interval,
                date,
                move |path| {
                    parses.fetch_add(1, Ordering::SeqCst);
                    Ok(std::fs::read_to_string(path)?.len())
                },
                move |order, files, _| {
                    folds.fetch_add(1, Ordering::SeqCst);
                    order
                        .iter()
                        .map(|path| *files[path].summary())
                        .sum::<usize>()
                },
            )
            .unwrap()
        };

        let first = scan(today, Duration::ZERO);
        assert_eq!(first.cache_status, CacheStatus::Refreshed);
        assert_eq!(first.report, 6);
        assert_eq!(parses.load(Ordering::SeqCst), 2);

        let hit = scan(today, Duration::ZERO);
        assert_eq!(hit.cache_status, CacheStatus::Hit);
        assert_eq!(parses.load(Ordering::SeqCst), 2);

        let mut file = OpenOptions::new().append(true).open(&first_path).unwrap();
        write!(file, "!").unwrap();
        let changed = scan(today, Duration::ZERO);
        assert_eq!(changed.cache_status, CacheStatus::Refreshed);
        assert_eq!(changed.report, 7);
        assert_eq!(parses.load(Ordering::SeqCst), 3);

        let tomorrow = today.checked_add_days(Days::new(1)).unwrap();
        let throttled = scan(tomorrow, Duration::from_secs(60));
        assert_eq!(throttled.cache_status, CacheStatus::Throttled);
        assert_eq!(parses.load(Ordering::SeqCst), 3);
        assert_eq!(folds.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn duplicate_and_overlapping_roots_do_not_double_count_files() {
        let dir = TestDir::new();
        let nested = dir.0.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("usage.jsonl"), "event").unwrap();
        let cache = Arc::new(Mutex::new(None));
        let result = scan_cached_files(
            cache,
            vec![dir.0.clone(), nested],
            "jsonl",
            0,
            Duration::ZERO,
            NaiveDate::from_ymd_opt(2026, 7, 11).unwrap(),
            |_| Ok(1usize),
            |order, _, _| order.len(),
        )
        .unwrap();

        assert_eq!(result.report, 1);
    }

    #[test]
    fn revision_changes_reparse_unchanged_files() {
        let dir = TestDir::new();
        std::fs::write(dir.0.join("usage.jsonl"), "event").unwrap();
        let cache = Arc::new(Mutex::new(None));
        let parses = Arc::new(AtomicUsize::new(0));

        for revision in [1, 2] {
            let parses = Arc::clone(&parses);
            let result = scan_cached_files(
                Arc::clone(&cache),
                vec![dir.0.clone()],
                "jsonl",
                revision,
                Duration::from_secs(60),
                NaiveDate::from_ymd_opt(2026, 7, 11).unwrap(),
                move |_| {
                    parses.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |order, _, _| order.len(),
            )
            .unwrap();
            assert_eq!(result.cache_status, CacheStatus::Refreshed);
        }

        assert_eq!(parses.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cache_mutex_is_released_while_parser_runs() {
        let dir = TestDir::new();
        std::fs::write(dir.0.join("usage.jsonl"), "event").unwrap();
        let cache = Arc::new(Mutex::<Option<LocalFileCache<(), usize>>>::new(None));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread_cache = cache.clone();
        let root = dir.0.clone();

        let scan = std::thread::spawn(move || {
            scan_cached_files(
                thread_cache,
                vec![root],
                "jsonl",
                1,
                Duration::ZERO,
                NaiveDate::from_ymd_opt(2026, 7, 11).unwrap(),
                move |_| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
                |order, _, _| order.len(),
            )
            .unwrap()
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(cache.try_lock().is_ok(), "parser held the cache mutex");
        release_tx.send(()).unwrap();
        assert_eq!(scan.join().unwrap().report, 1);
    }
}
