use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RenameMode};
use notify::{ErrorKind, Event, EventKind, RecursiveMode, Watcher};

use crate::watcher_filter::{
    derive_watcher_exclusion_plan, watcher_exclusion_paths, watcher_path_is_ignored_by_matcher,
    SharedGitignore, WATCHER_EXCLUSION_LIMIT,
};

const BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct ProjectWatcher {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    watched_directory_count: Arc<AtomicUsize>,
}

impl ProjectWatcher {
    pub(crate) fn create(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<Event>>,
        matcher: SharedGitignore,
        matcher_generation: Arc<AtomicU64>,
    ) -> notify::Result<Self> {
        Self::create_with_watch(
            root,
            extra_watch_paths,
            tx,
            matcher,
            matcher_generation,
            |watcher, path| watcher.watch(path, RecursiveMode::NonRecursive),
        )
    }

    fn create_with_watch<F>(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<Event>>,
        matcher: SharedGitignore,
        matcher_generation: Arc<AtomicU64>,
        mut watch: F,
    ) -> notify::Result<Self>
    where
        F: FnMut(&mut notify::RecommendedWatcher, &Path) -> notify::Result<()> + Send + 'static,
    {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        // The watch set below describes the matcher at this generation. Capture
        // it here, not on the backend thread: a bump between spawn and the
        // thread's first instruction would otherwise be read as "already
        // observed" and the rebuild it requires would never run.
        let observed_generation = matcher_generation.load(Ordering::Acquire);
        let plan = derive_watcher_exclusion_plan(&root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        let exclusions = plan.selected;
        let exclusion_paths = watcher_exclusion_paths(&exclusions);
        super::log_exclusions(&root, &exclusions);

        let (backend_tx, backend_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(backend_tx)?;
        let mut watched_directories = collect_watch_directories(&root, &matcher, &exclusion_paths);
        let counters = crate::context::watcher_counters_for_root(&root);
        counters.set_backend_exclusions(
            observed_generation,
            exclusion_paths.clone(),
            watcher_exclusion_paths(&plan.dropped),
        );
        let mut installed = BTreeSet::new();
        for directory in &watched_directories {
            if watch_directory(&mut watcher, directory, &mut watch, &counters)? {
                installed.insert(directory.clone());
            } else if directory == &root {
                return Err(notify::Error::path_not_found().add_path(root));
            }
        }
        watched_directories = installed;
        for path in extra_watch_paths {
            if path.exists() {
                let _ = watch_directory(&mut watcher, &path, &mut watch, &counters)?;
            }
        }

        let watched_directory_count = Arc::new(AtomicUsize::new(watched_directories.len()));
        let thread_count = Arc::clone(&watched_directory_count);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("aft-inotify-backend".to_string())
            .spawn(move || {
                let mut exclusions = exclusions;
                let mut exclusion_paths = exclusion_paths;
                let mut observed_generation = observed_generation;

                while !thread_shutdown.load(Ordering::Acquire) {
                    let generation = matcher_generation.load(Ordering::Acquire);
                    if generation != observed_generation {
                        let replacement_plan = derive_watcher_exclusion_plan(
                            &root,
                            &matcher,
                            Some(WATCHER_EXCLUSION_LIMIT),
                        );
                        let replacement_exclusions = replacement_plan.selected;
                        let replacement_paths = watcher_exclusion_paths(&replacement_exclusions);
                        let desired =
                            collect_watch_directories(&root, &matcher, &replacement_paths);
                        for directory in watched_directories.difference(&desired) {
                            let _ = watcher.unwatch(directory);
                        }
                        let mut installed = watched_directories
                            .intersection(&desired)
                            .cloned()
                            .collect::<BTreeSet<_>>();
                        for directory in desired.difference(&watched_directories) {
                            match watch_directory(&mut watcher, directory, &mut watch, &counters) {
                                Ok(true) => {
                                    installed.insert(directory.clone());
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    let _ = tx.send(Err(error));
                                    return;
                                }
                            }
                        }
                        watched_directories = installed;
                        thread_count.store(watched_directories.len(), Ordering::Release);
                        observed_generation = generation;

                        counters.set_backend_exclusions(
                            observed_generation,
                            replacement_paths.clone(),
                            watcher_exclusion_paths(&replacement_plan.dropped),
                        );
                        if replacement_exclusions != exclusions {
                            super::log_exclusions(&root, &replacement_exclusions);
                            exclusions = replacement_exclusions;
                        }
                        exclusion_paths = replacement_paths;
                    }

                    match backend_rx.recv_timeout(BACKEND_POLL_INTERVAL) {
                        Ok(Ok(event)) => {
                            // A move into the root is reported as Rename(To), not Create.
                            // Scan its descendants too so their later writes are watched.
                            if matches!(
                                event.kind,
                                EventKind::Create(CreateKind::Folder)
                                    | EventKind::Modify(ModifyKind::Name(RenameMode::To))
                            ) {
                                for path in &event.paths {
                                    for directory in
                                        collect_watch_directories(path, &matcher, &exclusion_paths)
                                    {
                                        if watched_directories.insert(directory.clone()) {
                                            match watch_directory(
                                                &mut watcher,
                                                &directory,
                                                &mut watch,
                                                &counters,
                                            ) {
                                                Ok(true) => {}
                                                Ok(false) => {
                                                    watched_directories.remove(&directory);
                                                }
                                                Err(error) => {
                                                    watched_directories.remove(&directory);
                                                    let _ = tx.send(Err(error));
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                }
                                thread_count.store(watched_directories.len(), Ordering::Release);
                            }
                            if tx.send(Ok(event)).is_err() {
                                return;
                            }
                        }
                        Ok(Err(error)) => {
                            if tx.send(Err(error)).is_err() {
                                return;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .map_err(notify::Error::io)?;

        Ok(Self {
            shutdown,
            join: Some(join),
            watched_directory_count,
        })
    }

    #[cfg(test)]
    fn watched_directory_count(&self) -> usize {
        self.watched_directory_count.load(Ordering::Acquire)
    }
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.watched_directory_count.store(0, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// A removed directory cannot contain live files to invalidate. A move out
// produces a rename event for its old path; a move into the root is scanned
// on Rename(To), including any files already present in its subtree.
fn watch_directory<F>(
    watcher: &mut notify::RecommendedWatcher,
    path: &Path,
    watch: &mut F,
    counters: &crate::context::WatcherCounters,
) -> notify::Result<bool>
where
    F: FnMut(&mut notify::RecommendedWatcher, &Path) -> notify::Result<()>,
{
    match watch(watcher, path) {
        Ok(()) => Ok(true),
        Err(error) => {
            // Limits and resource exhaustion are failures of the watcher, even
            // if the directory happens to disappear before the recheck.
            let fatal = matches!(error.kind, ErrorKind::MaxFilesWatch)
                || matches!(&error.kind, ErrorKind::Io(io) if matches!(io.raw_os_error(), Some(libc::ENOSPC | libc::EMFILE | libc::ENFILE)));
            if !fatal
                && (matches!(error.kind, ErrorKind::PathNotFound)
                    || matches!(&error.kind, ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                    || matches!(std::fs::metadata(path), Err(io) if io.kind() == std::io::ErrorKind::NotFound))
            {
                counters.note_watch_lost_race();
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

fn collect_watch_directories(
    root: &Path,
    matcher: &SharedGitignore,
    exclusions: &[PathBuf],
) -> BTreeSet<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut directories = BTreeSet::new();
    let mut stack = vec![root];

    while let Some(directory) = stack.pop() {
        if exclusions
            .iter()
            .any(|excluded| directory.starts_with(excluded))
            || watcher_path_is_ignored_by_matcher(matcher, &directory)
        {
            continue;
        }
        if !directory.is_dir() {
            continue;
        }
        directories.insert(directory.clone());
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    directories
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;
    use std::time::{Duration, Instant};

    use ignore::gitignore::GitignoreBuilder;

    use super::*;

    #[test]
    fn vanished_directory_does_not_stop_root_watch() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let vanished = canonical_root.join("ephemeral");
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create_with_watch(
            canonical_root.clone(),
            Vec::new(),
            tx,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
            {
                let vanished = vanished.clone();
                move |watcher, path| {
                    if path == vanished {
                        std::fs::remove_dir(&vanished).unwrap();
                    }
                    watcher.watch(path, RecursiveMode::NonRecursive)
                }
            },
        )
        .unwrap();
        std::fs::create_dir(&vanished).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while counters.snapshot().watch_lost_races_total == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(counters.snapshot().watch_lost_races_total, 1);
        assert_eq!(watcher.watched_directory_count(), 1);

        let later = canonical_root.join("later.txt");
        std::fs::write(&later, "still watched").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("root event after vanished directory")
                .expect("backend should remain available");
            if event.paths.contains(&later) {
                break;
            }
        }
    }

    #[test]
    fn watch_limit_error_remains_fatal() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create_with_watch(
            canonical_root.clone(),
            Vec::new(),
            tx,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
            move |watcher, path| {
                if path.ends_with("limit") {
                    return Err(notify::Error::io(std::io::Error::from_raw_os_error(
                        libc::ENOSPC,
                    )));
                }
                watcher.watch(path, RecursiveMode::NonRecursive)
            },
        )
        .unwrap();
        std::fs::create_dir(canonical_root.join("limit")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("fatal watcher error")
            {
                Err(error) => {
                    assert!(
                        matches!(error.kind, ErrorKind::Io(ref io) if io.raw_os_error() == Some(libc::ENOSPC))
                    );
                    break;
                }
                Ok(_) => {}
            }
        }
        assert_eq!(
            crate::context::watcher_counters_for_root(&canonical_root)
                .snapshot()
                .watch_lost_races_total,
            0
        );
        drop(watcher);
    }

    #[test]
    fn inotify_walk_does_not_watch_ignored_subtrees() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("target/nested")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _rx) = mpsc::channel();

        let watcher =
            ProjectWatcher::create(canonical_root, Vec::new(), tx, matcher, generation).unwrap();

        assert_eq!(watcher.watched_directory_count(), 2);
    }

    // The matcher bump here can land before the backend thread executes its
    // first instruction (it did on a loaded Linux CI runner). The watch set
    // must still be rebuilt, which is why `create` captures the generation
    // alongside the initial walk instead of on the thread.
    #[test]
    fn inotify_rebuild_adjusts_existing_watches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            canonical_root.clone(),
            Vec::new(),
            tx,
            Arc::clone(&matcher),
            Arc::clone(&generation),
        )
        .unwrap();
        assert_eq!(watcher.watched_directory_count(), 2);

        std::fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        *matcher.write().unwrap() = Some(Arc::new(builder.build().unwrap()));
        generation.fetch_add(1, Ordering::Release);

        let deadline = Instant::now() + Duration::from_secs(2);
        while watcher.watched_directory_count() != 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(watcher.watched_directory_count(), 1);
    }

    #[test]
    fn inotify_skips_ignored_directory_created_after_bind() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            canonical_root.clone(),
            Vec::new(),
            tx,
            Arc::clone(&matcher),
            generation,
        )
        .unwrap();
        assert_eq!(watcher.watched_directory_count(), 1);

        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::create_dir(root.path().join("target/not-watched")).unwrap();
        std::fs::create_dir(root.path().join("new-source")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while watcher.watched_directory_count() != 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(watcher.watched_directory_count(), 2);
    }

    #[test]
    fn inotify_adds_only_new_nonignored_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _rx) = mpsc::channel();
        let watcher =
            ProjectWatcher::create(canonical_root, Vec::new(), tx, matcher, generation).unwrap();

        std::fs::create_dir(root.path().join("new-source")).unwrap();
        std::fs::create_dir(root.path().join("target/not-watched")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while watcher.watched_directory_count() != 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(watcher.watched_directory_count(), 2);
    }
}
