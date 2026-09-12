use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::event::CreateKind;
use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::watcher_filter::{
    derive_excluded_subtrees, watcher_path_is_ignored_by_matcher, SharedGitignore,
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
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let exclusions = derive_excluded_subtrees(&root, &matcher, None);
        super::log_exclusions(&root, &exclusions);

        let (backend_tx, backend_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(backend_tx)?;
        let mut watched_directories = collect_watch_directories(&root, &matcher);
        for directory in &watched_directories {
            watcher.watch(directory, RecursiveMode::NonRecursive)?;
        }
        for path in extra_watch_paths {
            if path.exists() {
                watcher.watch(&path, RecursiveMode::NonRecursive)?;
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
                let mut observed_generation = matcher_generation.load(Ordering::Acquire);

                while !thread_shutdown.load(Ordering::Acquire) {
                    let generation = matcher_generation.load(Ordering::Acquire);
                    if generation != observed_generation {
                        let desired = collect_watch_directories(&root, &matcher);
                        for directory in watched_directories.difference(&desired) {
                            let _ = watcher.unwatch(directory);
                        }
                        for directory in desired.difference(&watched_directories) {
                            if let Err(error) =
                                watcher.watch(directory, RecursiveMode::NonRecursive)
                            {
                                let _ = tx.send(Err(error));
                                return;
                            }
                        }
                        watched_directories = desired;
                        thread_count.store(watched_directories.len(), Ordering::Release);
                        observed_generation = generation;

                        let replacement_exclusions =
                            derive_excluded_subtrees(&root, &matcher, None);
                        if replacement_exclusions != exclusions {
                            super::log_exclusions(&root, &replacement_exclusions);
                            exclusions = replacement_exclusions;
                        }
                    }

                    match backend_rx.recv_timeout(BACKEND_POLL_INTERVAL) {
                        Ok(Ok(event)) => {
                            if matches!(event.kind, EventKind::Create(CreateKind::Folder)) {
                                for path in &event.paths {
                                    for directory in collect_watch_directories(path, &matcher) {
                                        if watched_directories.insert(directory.clone()) {
                                            if let Err(error) = watcher
                                                .watch(&directory, RecursiveMode::NonRecursive)
                                            {
                                                watched_directories.remove(&directory);
                                                let _ = tx.send(Err(error));
                                                return;
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

fn collect_watch_directories(root: &Path, matcher: &SharedGitignore) -> BTreeSet<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut directories = BTreeSet::new();
    let mut stack = vec![root];

    while let Some(directory) = stack.pop() {
        if watcher_path_is_ignored_by_matcher(matcher, &directory) {
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
