use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, Flag, ModifyKind, RenameMode};
use notify::{ErrorKind, Event, EventKind, RecursiveMode, Watcher};

use crate::watcher_filter::{
    derive_watcher_exclusion_plan, start_watcher_rebuild, watcher_directory_is_ignored_by_matcher,
    watcher_exclusion_paths, watcher_path_is_ignored_by_matcher, ExclusionPlanUnavailable,
    SharedGitignore, WATCHER_EXCLUSION_LIMIT,
};

const BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Most files reported for one batch of newly watched directories. Past it
/// the backend asks for a rescan instead of a partial list.
const NEW_DIRECTORY_FILE_CAP: usize = 4_096;
/// Longest one enumeration of newly watched directories may take before the
/// backend gives up and asks for a rescan.
const NEW_DIRECTORY_SCAN_BUDGET: Duration = Duration::from_millis(200);
/// How long files found in a new directory wait for the kernel's own events
/// for the same paths before they are reported.
const NEW_DIRECTORY_REPORT_DELAY: Duration = Duration::from_millis(100);

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
        watch: F,
    ) -> notify::Result<Self>
    where
        F: FnMut(&mut notify::RecommendedWatcher, &Path) -> notify::Result<()> + Send + 'static,
    {
        Self::create_with_plan(
            root,
            extra_watch_paths,
            tx,
            matcher,
            matcher_generation,
            watch,
            |root, matcher| {
                derive_watcher_exclusion_plan(root, matcher, Some(WATCHER_EXCLUSION_LIMIT))
            },
        )
    }

    fn create_with_plan<F, P>(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<Event>>,
        matcher: SharedGitignore,
        matcher_generation: Arc<AtomicU64>,
        mut watch: F,
        mut derive: P,
    ) -> notify::Result<Self>
    where
        F: FnMut(&mut notify::RecommendedWatcher, &Path) -> notify::Result<()> + Send + 'static,
        P: FnMut(
                &Path,
                &SharedGitignore,
            ) -> Result<
                crate::watcher_filter::WatcherExclusionPlan,
                crate::watcher_filter::ExclusionPlanUnavailable,
            > + Send
            + 'static,
    {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        // The watch set below describes the matcher at this generation. Capture
        // it here, not on the backend thread: a bump between spawn and the
        // thread's first instruction would otherwise be read as "already
        // observed" and the rebuild it requires would never run.
        let generation = matcher_generation.load(Ordering::Acquire);
        let counters = crate::context::watcher_counters_for_root(&root);
        let mut planning = crate::watcher_filter::WatcherExclusionRetry::default();
        let outcome = planning
            .attempt(&root, generation, &counters, || derive(&root, &matcher))
            .expect("initial exclusion attempt is due");
        let observed_generation = outcome.is_ok().then_some(generation);
        let plan = outcome.unwrap_or_default();
        let exclusions = plan.selected;
        let exclusion_state = crate::watcher_filter::WatcherExclusionPaths::new(&root, &exclusions);
        let exclusion_paths = exclusion_state.paths();
        super::log_exclusions(&root, &exclusions, generation);

        let (backend_tx, backend_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(backend_tx)?;
        let walked_directories = exclusion_state
            .boundary()
            .map_or_else(BTreeMap::new, |boundary| {
                collect_watch_directories(boundary, &matcher, &exclusion_paths)
            });
        counters.set_backend_exclusions(
            observed_generation.unwrap_or(0),
            exclusion_paths.clone(),
            watcher_exclusion_paths(&plan.dropped),
        );
        let mut installed = BTreeSet::new();
        for directory in desired_watch_directories(
            walked_directories,
            &BTreeSet::new(),
            &matcher,
            &exclusion_paths,
            &root,
        ) {
            if watch_directory(&mut watcher, &directory, &mut watch, &counters)? {
                installed.insert(directory.clone());
            } else if directory == root {
                return Err(notify::Error::path_not_found().add_path(root));
            }
        }
        let mut watched_directories = installed;
        let git_directory = root.join(".git");
        let mut git_watched = watched_directories.contains(&git_directory);
        for path in extra_watch_paths {
            if path.exists() {
                let _ = watch_directory(&mut watcher, &path, &mut watch, &counters)?;
                if path == git_directory {
                    git_watched = true;
                }
            }
        }
        counters.bash_flush.set_marker_directory(
            if git_watched
                && std::fs::symlink_metadata(&git_directory).is_ok_and(|metadata| metadata.is_dir())
            {
                git_directory
            } else {
                root.clone()
            },
        );

        let watched_directory_count = Arc::new(AtomicUsize::new(watched_directories.len()));
        let thread_count = Arc::clone(&watched_directory_count);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("aft-inotify-backend".to_string())
            .spawn(move || {
                let mut exclusions = exclusions;
                let mut exclusion_paths = exclusion_paths;
                let mut exclusion_state = exclusion_state;
                let mut dropped_paths = watcher_exclusion_paths(&plan.dropped);
                let mut observed_generation = observed_generation;
                // Files found in directories that appeared after the watch was
                // set up, waiting to be reported. inotify watches each
                // directory on its own, and a new directory's watch is added
                // only after its create event arrives, so anything written
                // into it before then produces no event at all (a checkout or
                // a generator that creates a directory and fills it straight
                // away). Enumerating the directory once recovers those files.
                let mut new_directory_files = PendingNewDirectoryFiles::default();
                let mut pending_rebuild: Option<PendingWatchRebuild> = None;
                let mut rebuild_needed = false;

                while !thread_shutdown.load(Ordering::Acquire) {
                    if new_directory_files.due() {
                        if tx.send(Ok(new_directory_files.take_event())).is_err() {
                            return;
                        }
                    }
                    let generation = matcher_generation.load(Ordering::Acquire);
                    if pending_rebuild
                        .as_ref()
                        .is_some_and(|work| work.generation != generation)
                    {
                        // An old matcher's result must not restore its watch set.
                        pending_rebuild = None;
                    }
                    if let Some(outcome) = pending_rebuild.as_ref().and_then(|work| work.poll()) {
                        let work = pending_rebuild
                            .take()
                            .expect("completed rebuild is pending");
                        match outcome {
                            Ok(desired) => {
                                // Create events can install replacement watches after the
                                // helper recorded the old inode. Keep those installations
                                // separate so the stale snapshot cannot evict them.
                                let concurrent_watches = work
                                    .installed
                                    .intersection(&watched_directories)
                                    .cloned()
                                    .collect::<BTreeSet<_>>();
                                let mut desired = desired_watch_directories(
                                    desired,
                                    &watched_directories,
                                    &matcher,
                                    &exclusion_paths,
                                    &root,
                                );
                                desired.extend(concurrent_watches.iter().cloned());
                                watched_directories = match reconcile_watch_directories(
                                    &watched_directories,
                                    &desired,
                                    |directory, install| {
                                        if install {
                                            // These watches are already live. Reopening their
                                            // paths could follow a subsequent symlink replacement.
                                            if concurrent_watches.contains(directory) {
                                                return Ok(true);
                                            }
                                            watch_directory(
                                                &mut watcher,
                                                directory,
                                                &mut watch,
                                                &counters,
                                            )
                                        } else {
                                            let _ = watcher.unwatch(directory);
                                            Ok(false)
                                        }
                                    },
                                ) {
                                    Ok(installed) => installed,
                                    Err(error) => {
                                        let _ = tx.send(Err(error));
                                        return;
                                    }
                                };
                                thread_count.store(watched_directories.len(), Ordering::Release);
                                rebuild_needed = false;
                            }
                            Err(reason) => {
                                planning.walk_unavailable(&root, generation, &counters, reason)
                            }
                        }
                    }
                    let mut attempted = false;
                    let update = if pending_rebuild.is_none() {
                        planning.replacement(
                            &root,
                            generation,
                            &counters,
                            &mut exclusion_state,
                            &matcher,
                            || {
                                attempted = true;
                                derive(&root, &matcher)
                            },
                        )
                    } else {
                        None
                    };
                    if let Some(update) = update {
                        let replacement_paths = exclusion_state.paths();
                        let replacement_exclusions = match update {
                            crate::watcher_filter::WatcherExclusionUpdate::Planned(plan) => {
                                dropped_paths = watcher_exclusion_paths(&plan.dropped);
                                plan.selected
                            }
                            crate::watcher_filter::WatcherExclusionUpdate::Reduced => exclusions
                                .iter()
                                .filter(|entry| {
                                    replacement_paths.iter().any(|path| path == entry.path())
                                })
                                .cloned()
                                .collect(),
                        };
                        forget_removed_watch_paths(
                            &mut watched_directories,
                            &replacement_paths,
                            |directory| {
                                let _ = watcher.unwatch(directory);
                            },
                        );
                        thread_count.store(watched_directories.len(), Ordering::Release);
                        rebuild_needed = true;
                        observed_generation = Some(generation);

                        counters.set_backend_exclusions(
                            observed_generation.unwrap_or(0),
                            replacement_paths.clone(),
                            dropped_paths.clone(),
                        );
                        if replacement_exclusions != exclusions {
                            super::log_exclusions(&root, &replacement_exclusions, generation);
                            exclusions = replacement_exclusions;
                        }
                        exclusion_paths = replacement_paths;
                    }
                    if attempted && rebuild_needed {
                        let boundary = exclusion_state.boundary().cloned();
                        let walk_matcher = matcher.clone();
                        let walk_exclusions = exclusion_paths.clone();
                        match start_watcher_rebuild(move || {
                            boundary.map_or_else(BTreeMap::new, |boundary| {
                                collect_watch_directories(
                                    &boundary,
                                    &walk_matcher,
                                    &walk_exclusions,
                                )
                            })
                        }) {
                            Ok(result) => {
                                pending_rebuild = Some(PendingWatchRebuild {
                                    generation,
                                    result,
                                    installed: BTreeSet::new(),
                                })
                            }
                            Err(reason) => {
                                planning.walk_unavailable(&root, generation, &counters, reason)
                            }
                        }
                    }

                    match backend_rx.recv_timeout(BACKEND_POLL_INTERVAL) {
                        Ok(Ok(event)) => {
                            if event.paths.iter().any(|path| {
                                super::cookie::is_cookie_path(path)
                                    || (counters.bash_changes.get().is_some()
                                        && super::flush::is_marker_path(path))
                            }) {
                                // New-directory recovery waits briefly for duplicate kernel
                                // events. Publish its files before the ordered marker too.
                                if !new_directory_files.files.is_empty()
                                    && tx.send(Ok(new_directory_files.take_event())).is_err()
                                {
                                    return;
                                }
                            }
                            let changed_paths = exclusion_state.refresh_event(&event);
                            if !changed_paths.is_empty() {
                                exclusion_paths = exclusion_state.paths();
                                counters.set_backend_exclusions(
                                    observed_generation.unwrap_or(0),
                                    exclusion_paths.clone(),
                                    dropped_paths.clone(),
                                );
                                forget_removed_watch_paths(
                                    &mut watched_directories,
                                    &exclusion_paths,
                                    |directory| {
                                        let _ = watcher.unwatch(directory);
                                    },
                                );
                            }
                            if matches!(
                                event.kind,
                                EventKind::Remove(_)
                                    | EventKind::Modify(ModifyKind::Name(
                                        RenameMode::From | RenameMode::Both
                                    ))
                            ) {
                                let removed = if matches!(
                                    event.kind,
                                    EventKind::Modify(ModifyKind::Name(RenameMode::Both))
                                ) {
                                    &event.paths[..event.paths.len().min(1)]
                                } else {
                                    &event.paths[..]
                                };
                                forget_removed_watch_paths(
                                    &mut watched_directories,
                                    removed,
                                    |directory| {
                                        let _ = watcher.unwatch(directory);
                                    },
                                );
                                thread_count.store(watched_directories.len(), Ordering::Release);
                            }
                            let scan_paths = new_tree_event_paths(&event, &changed_paths);
                            if let Some(boundary) = exclusion_state
                                .boundary()
                                .filter(|_| !scan_paths.is_empty())
                            {
                                let deadline = Instant::now() + NEW_DIRECTORY_SCAN_BUDGET;
                                let mut scan = NewDirectoryScan::Complete;
                                for path in &scan_paths {
                                    match watch_new_tree(
                                        path,
                                        boundary,
                                        &matcher,
                                        &exclusion_paths,
                                        &mut watched_directories,
                                        &mut new_directory_files,
                                        NEW_DIRECTORY_FILE_CAP,
                                        deadline,
                                        |directory| {
                                            let installed = watch_directory(
                                                &mut watcher,
                                                directory,
                                                &mut watch,
                                                &counters,
                                            )?;
                                            if installed {
                                                if let Some(work) = pending_rebuild.as_mut() {
                                                    work.installed.insert(directory.to_path_buf());
                                                }
                                            }
                                            Ok(installed)
                                        },
                                    ) {
                                        Ok(NewDirectoryScan::Complete) => {}
                                        Ok(NewDirectoryScan::Bounded) => {
                                            scan = NewDirectoryScan::Bounded;
                                        }
                                        Err(error) => {
                                            let _ = tx.send(Err(error));
                                            return;
                                        }
                                    }
                                }
                                thread_count.store(watched_directories.len(), Ordering::Release);
                                if scan == NewDirectoryScan::Bounded {
                                    // Too many files or too slow: ask for a
                                    // rescan rather than report part of them.
                                    let rescan =
                                        Event::new(EventKind::Other).set_flag(Flag::Rescan);
                                    if tx.send(Ok(rescan)).is_err() {
                                        return;
                                    }
                                }
                            }
                            // The kernel reports this path itself, so the
                            // enumeration need not.
                            new_directory_files.forget(&event.paths);
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

struct PendingWatchRebuild {
    generation: u64,
    result: mpsc::Receiver<Result<BTreeMap<PathBuf, DirectoryIdentity>, ExclusionPlanUnavailable>>,
    installed: BTreeSet<PathBuf>,
}

impl PendingWatchRebuild {
    fn poll(
        &self,
    ) -> Option<Result<BTreeMap<PathBuf, DirectoryIdentity>, ExclusionPlanUnavailable>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(ExclusionPlanUnavailable::WorkerDisconnected))
            }
        }
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

/// Outcome of enumerating newly watched directories.
#[derive(Debug, PartialEq, Eq)]
enum NewDirectoryScan {
    /// Every file was found and queued.
    Complete,
    /// The file cap or the time budget stopped the walk; the caller asks for
    /// a rescan instead.
    Bounded,
}

/// Files found by enumerating directories that appeared after the watch was
/// set up, held briefly so a kernel event for the same path replaces them.
#[derive(Default)]
struct PendingNewDirectoryFiles {
    files: BTreeSet<PathBuf>,
    since: Option<Instant>,
}

impl PendingNewDirectoryFiles {
    fn queue(&mut self, path: PathBuf) {
        if self.files.insert(path) && self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }

    /// Drop paths the kernel reported itself.
    fn forget(&mut self, paths: &[PathBuf]) {
        for path in paths {
            self.files.remove(path);
        }
        if self.files.is_empty() {
            self.since = None;
        }
    }

    fn due(&self) -> bool {
        self.since
            .is_some_and(|since| since.elapsed() >= NEW_DIRECTORY_REPORT_DELAY)
    }

    /// One create event naming every queued file.
    fn take_event(&mut self) -> Event {
        self.since = None;
        let mut event = Event::new(EventKind::Create(CreateKind::File));
        for path in std::mem::take(&mut self.files) {
            event = event.add_path(path);
        }
        event
    }
}

fn new_tree_event_paths(event: &Event, changed_external_paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut paths: BTreeSet<_> = changed_external_paths.iter().cloned().collect();
    // Only directory creates and untyped renames can introduce a local tree.
    // A remove needs a scan only when an external boundary changed: the old
    // link may already have been replaced before its remove event is handled.
    if matches!(
        event.kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Modify(ModifyKind::Name(_))
    ) {
        paths.extend(event.paths.iter().cloned());
    }
    paths
}

/// Watches `root` and every directory under it that is not watched yet, and
/// queues the non-ignored files found in them.
///
/// Each directory's watch is installed before the directory is read. Then a
/// child created after the read is reported by the kernel, and one created
/// before it is found by the read, so nothing falls between the two. Reading
/// first leaves a gap: a subdirectory created after the read but before its
/// parent's watch was installed would never be watched at all.
///
/// Every new directory is still watched. File listing stops at `cap` files or
/// at `deadline`, and the result is then `Bounded` so the caller can ask for a
/// rescan instead of reporting a partial list.
fn watch_new_tree(
    root: &Path,
    boundary: &crate::watcher_filter::WatcherWalkBoundary,
    matcher: &SharedGitignore,
    exclusions: &[PathBuf],
    watched: &mut BTreeSet<PathBuf>,
    pending: &mut PendingNewDirectoryFiles,
    cap: usize,
    deadline: Instant,
    mut watch: impl FnMut(&Path) -> notify::Result<bool>,
) -> notify::Result<NewDirectoryScan> {
    let project_root = boundary.root();
    let excluded = |path: &Path, is_dir| {
        exclusions.iter().any(|excluded| path.starts_with(excluded))
            || if is_dir {
                watcher_directory_is_ignored_by_matcher(matcher, path, project_root)
            } else {
                watcher_path_is_ignored_by_matcher(matcher, path, project_root)
            }
    };
    let mut stack = vec![root.to_path_buf()];
    let mut found = 0usize;
    let mut scan = NewDirectoryScan::Complete;
    while let Some(directory) = stack.pop() {
        if excluded(&directory, true)
            || !boundary.allows_directory(&directory)
            || !watched.insert(directory.clone())
        {
            continue;
        }
        match watch(&directory) {
            Ok(true) => {}
            Ok(false) => {
                watched.remove(&directory);
                continue;
            }
            Err(error) => {
                watched.remove(&directory);
                return Err(error);
            }
        }
        let Ok(entries) = crate::watcher_filter::exclusion_read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                stack.push(path);
                continue;
            }
            if scan == NewDirectoryScan::Bounded {
                continue;
            }
            if found >= cap || Instant::now() >= deadline {
                scan = NewDirectoryScan::Bounded;
                continue;
            }
            if excluded(&path, false) {
                continue;
            }
            found += 1;
            pending.queue(path);
        }
    }
    Ok(scan)
}

fn forget_removed_watch_paths(
    watched: &mut BTreeSet<PathBuf>,
    paths: &[PathBuf],
    mut unwatch: impl FnMut(&Path),
) {
    let removed = watched
        .iter()
        .filter(|directory| paths.iter().any(|path| directory.starts_with(path)))
        .cloned()
        .collect::<Vec<_>>();
    for directory in removed {
        unwatch(&directory);
        watched.remove(&directory);
    }
}

fn reconcile_watch_directories(
    previous: &BTreeSet<PathBuf>,
    desired: &BTreeSet<PathBuf>,
    mut update: impl FnMut(&Path, bool) -> notify::Result<bool>,
) -> notify::Result<BTreeSet<PathBuf>> {
    // A retained path may now name a different inode after delete/recreate,
    // so every desired path is watched again rather than trusted from the
    // bookkeeping. Re-adding a watch on an unchanged inode only refreshes its
    // mask, so retained directories are never unwatched and no event window
    // opens during the rebuild.
    for directory in previous.difference(desired) {
        let _ = update(directory, false);
    }
    let mut installed = BTreeSet::new();
    for directory in desired {
        if update(directory, true)? {
            installed.insert(directory.clone());
        }
    }
    Ok(installed)
}

#[derive(Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn read(path: &Path) -> Option<Self> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        metadata.is_dir().then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

fn collect_watch_directories(
    boundary: &crate::watcher_filter::WatcherWalkBoundary,
    matcher: &SharedGitignore,
    exclusions: &[PathBuf],
) -> BTreeMap<PathBuf, DirectoryIdentity> {
    let root = boundary.root().to_path_buf();
    let mut directories = BTreeMap::new();
    let mut stack = vec![root.clone()];

    while let Some(directory) = stack.pop() {
        if exclusions
            .iter()
            .any(|excluded| directory.starts_with(excluded))
            || watcher_directory_is_ignored_by_matcher(matcher, &directory, &root)
        {
            continue;
        }
        if !boundary.allows_directory(&directory) {
            continue;
        }
        let Some(identity) = DirectoryIdentity::read(&directory) else {
            continue;
        };
        directories.insert(directory.clone(), identity);
        #[cfg(test)]
        tests::block_watch_walk(&directory);
        let Ok(entries) = crate::watcher_filter::exclusion_read_dir(&directory) else {
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

fn desired_watch_directories(
    walked: BTreeMap<PathBuf, DirectoryIdentity>,
    watched: &BTreeSet<PathBuf>,
    matcher: &SharedGitignore,
    exclusions: &[PathBuf],
    root: &Path,
) -> BTreeSet<PathBuf> {
    let mut desired = BTreeSet::new();
    let mut stale = Vec::new();
    // Parents sort before descendants. Recheck each parent before lstat of its
    // children, since even lstat follows symlinks in ancestor components.
    for (directory, identity) in walked {
        if (directory == root
            || directory
                .parent()
                .is_some_and(|path| desired.contains(path)))
            && DirectoryIdentity::read(&directory).as_ref() == Some(&identity)
        {
            desired.insert(directory);
        } else {
            stale.push(directory);
        }
    }
    // A previously watched path is not proof of a replacement's identity.
    // Watches installed during the walk are preserved separately on apply.
    desired.extend(
        watched
            .iter()
            .filter(|path| !stale.iter().any(|changed| path.starts_with(changed)))
            .cloned(),
    );
    desired.retain(|directory| {
        !exclusions.iter().any(|path| directory.starts_with(path))
            && !watcher_directory_is_ignored_by_matcher(matcher, directory, root)
    });
    desired
}

#[cfg(test)]
pub(super) mod tests {
    #[test]
    fn watcher_cookie_inotify_recovery_precedes_root_cookie_without_command_tracking() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let tree = staging.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("source.rs"), b"already populated").unwrap();
        let counters = crate::context::watcher_counters_for_root(&root);
        assert!(counters.bash_changes.get().is_none());
        let (tx, rx) = mpsc::channel();
        let _watcher = ProjectWatcher::create(
            root.clone(),
            vec![],
            tx,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        std::fs::rename(&tree, root.join("tree")).unwrap();
        let cookie = root.join(".aft-cookie-123-nonce");
        std::fs::write(&cookie, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut recovered = false;
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
                .unwrap();
            assert!(!event.need_rescan());
            recovered |= event.paths.contains(&root.join("tree/source.rs"));
            if event.paths.contains(&cookie) {
                assert!(
                    recovered,
                    "cookie overtook recovered new-directory membership"
                );
                break;
            }
        }
    }

    use std::sync::{Condvar, Mutex, OnceLock, RwLock};
    use std::time::{Duration, Instant};

    use ignore::gitignore::GitignoreBuilder;

    #[derive(Default)]
    struct WalkGate {
        state: Mutex<(bool, usize)>,
        changed: Condvar,
    }

    fn walk_gates() -> &'static Mutex<std::collections::BTreeMap<PathBuf, Arc<WalkGate>>> {
        static GATES: OnceLock<Mutex<std::collections::BTreeMap<PathBuf, Arc<WalkGate>>>> =
            OnceLock::new();
        GATES.get_or_init(Mutex::default)
    }

    pub(crate) fn block_watch_walk(path: &Path) {
        let gate = walk_gates().lock().unwrap().get(path).cloned();
        if let Some(gate) = gate {
            let mut state = gate.state.lock().unwrap();
            state.1 += 1;
            gate.changed.notify_all();
            while !state.0 {
                state = gate.changed.wait(state).unwrap();
            }
        }
    }

    struct WalkBlock {
        path: PathBuf,
        gate: Arc<WalkGate>,
    }

    impl WalkBlock {
        fn new(path: PathBuf) -> Self {
            let gate = Arc::new(WalkGate::default());
            walk_gates()
                .lock()
                .unwrap()
                .insert(path.clone(), gate.clone());
            Self { path, gate }
        }

        fn release(&self) {
            self.gate.state.lock().unwrap().0 = true;
            self.gate.changed.notify_all();
        }

        fn wait_for_walks(&self, count: usize) {
            let (state, _) = self
                .gate
                .changed
                .wait_timeout_while(
                    self.gate.state.lock().unwrap(),
                    Duration::from_secs(5),
                    |state| state.1 < count,
                )
                .unwrap();
            let started = state.1;
            drop(state);
            assert!(started >= count, "re-inclusion walk {count} never started");
        }
    }

    impl Drop for WalkBlock {
        fn drop(&mut self) {
            self.release();
            walk_gates().lock().unwrap().remove(&self.path);
        }
    }

    struct ReincludeFixture {
        // Release blocked helpers before joining the watcher, including on panic.
        block: WalkBlock,
        watcher: ProjectWatcher,
        root: tempfile::TempDir,
        rx: mpsc::Receiver<notify::Result<Event>>,
        watch_rx: mpsc::Receiver<PathBuf>,
        counters: Arc<crate::context::WatcherCounters>,
    }

    impl ReincludeFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("generated/nested")).unwrap();
            std::fs::create_dir(root.path().join("healthy")).unwrap();
            let canonical_root = std::fs::canonicalize(root.path()).unwrap();
            std::fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
            let mut builder = GitignoreBuilder::new(&canonical_root);
            builder.add(root.path().join(".gitignore"));
            let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
            let generation = Arc::new(AtomicU64::new(1));
            let counters = crate::context::watcher_counters_for_root(&canonical_root);
            let (tx, rx) = mpsc::channel();
            let (watch_tx, watch_rx) = mpsc::channel();
            let mut initial = true;
            let watcher = ProjectWatcher::create_with_plan(
                canonical_root.clone(),
                Vec::new(),
                tx,
                matcher.clone(),
                generation.clone(),
                move |watcher, path| {
                    watcher.watch(path, RecursiveMode::NonRecursive)?;
                    let _ = watch_tx.send(path.to_path_buf());
                    Ok(())
                },
                move |root, matcher| {
                    if std::mem::take(&mut initial) {
                        derive_watcher_exclusion_plan(root, matcher, Some(WATCHER_EXCLUSION_LIMIT))
                    } else {
                        Err(crate::watcher_filter::ExclusionPlanUnavailable::WorkerDisconnected)
                    }
                },
            )
            .unwrap();
            assert_eq!(watcher.watched_directory_count(), 2);
            while watch_rx.try_recv().is_ok() {}
            let path = canonical_root.join("generated");
            let gate = Arc::new(WalkGate::default());
            walk_gates()
                .lock()
                .unwrap()
                .insert(path.clone(), gate.clone());
            let block = WalkBlock { path, gate };
            *matcher.write().unwrap() = None;
            generation.fetch_add(1, Ordering::Release);
            block.wait_for_walks(1);
            Self {
                block,
                watcher,
                root,
                rx,
                watch_rx,
                counters,
            }
        }

        fn wait_until_watched(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.watcher.watched_directory_count() != 4 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(self.watcher.watched_directory_count(), 4);
        }
    }

    #[test]
    fn reinclude_blocked_walk_keeps_healthy_events_flowing() {
        let fixture = ReincludeFixture::new();
        let path = fixture.root.path().join("healthy/still-watched.rs");
        std::fs::write(&path, "source").unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let event = fixture
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("healthy folder event stalled behind the re-inclusion walk")
                .unwrap();
            if event.paths.contains(&path) {
                break;
            }
        }
        assert!(!fixture.block.gate.state.lock().unwrap().0);
        fixture.block.release();
        fixture.wait_until_watched();
    }

    #[test]
    fn reinclude_completed_walk_restores_descendant_events() {
        let fixture = ReincludeFixture::new();
        fixture.block.release();
        fixture.wait_until_watched();
        let path = fixture.root.path().join("generated/nested/restored.rs");
        std::fs::write(&path, "source").unwrap();
        wait_for_paths(&fixture.rx, &[path]);
    }

    #[test]
    fn reinclude_completed_walk_keeps_recreated_directory_watches() {
        let fixture = ReincludeFixture::new();
        let directory = fixture.root.path().join("generated");
        let nested = directory.join("nested");
        // Keep the old inode allocated so delete/recreate cannot reuse it.
        let _old_directory = std::fs::File::open(&directory).unwrap();
        let old_identity = DirectoryIdentity::read(&directory).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_ne!(DirectoryIdentity::read(&directory), Some(old_identity));
        let files = [directory.join("direct.rs"), nested.join("descendant.rs")];
        for path in &files {
            std::fs::write(path, "before rebuild").unwrap();
        }
        wait_for_paths(&fixture.rx, &[directory, nested]);
        fixture.wait_until_watched();

        fixture.block.release();
        // Reconciliation reinstalls healthy last, after deciding which of the
        // replacement watches to retain. A watch count alone is already four
        // before the helper finishes and cannot acknowledge its application.
        let healthy = fixture.root.path().join("healthy");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let installed = fixture
                .watch_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("completed helper walk was never reconciled");
            if installed == healthy {
                break;
            }
        }
        // Drain earlier kernel events before editing the same files again.
        let barrier = healthy.join("after-rebuild.rs");
        std::fs::write(&barrier, "barrier").unwrap();
        wait_for_paths(&fixture.rx, &[barrier]);
        for path in &files {
            std::fs::write(path, "after rebuild").unwrap();
            wait_for_event(
                &fixture.rx,
                &format!("post-rebuild edit of {path:?}"),
                |event| {
                    matches!(event.kind, EventKind::Modify(ModifyKind::Data(_)))
                        && event.paths.contains(path)
                },
            );
        }
    }

    #[test]
    fn reinclude_walk_timeout_is_visible_and_retried() {
        let fixture = ReincludeFixture::new();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let health = fixture.counters.snapshot().exclusion_planning.unwrap();
            if health.last_failure == Some("io_deadline") {
                assert_eq!(health.status, "retrying");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "walk timeout missing from health: {health:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fixture.watcher.watched_directory_count(), 2);
        fixture.block.wait_for_walks(2);
        fixture.block.release();
        fixture.wait_until_watched();
    }

    #[test]
    fn completed_walk_rejects_symlink_replacement_before_apply() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let directory = root.join("collected");
        let child = directory.join("nested");
        std::fs::create_dir_all(&child).unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(
            GitignoreBuilder::new(&root).build().unwrap(),
        ))));
        let walked = collect_watch_directories(
            &crate::watcher_filter::WatcherWalkBoundary::for_root(&root).unwrap(),
            &matcher,
            &[],
        );
        let target = tempfile::tempdir().unwrap();
        let moved = target.path().join("moved");
        std::fs::rename(&directory, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &directory).unwrap();
        let new_directory = root.join("newly-watched");
        std::fs::create_dir(&new_directory).unwrap();
        let desired = desired_watch_directories(
            walked,
            &BTreeSet::from([directory.clone(), child.clone(), new_directory.clone()]),
            &matcher,
            &[],
            &root,
        );
        let mut installed = BTreeSet::new();
        reconcile_watch_directories(&BTreeSet::new(), &desired, |path, install| {
            if install {
                installed.insert(path.to_path_buf());
            }
            Ok(install)
        })
        .unwrap();
        assert!(installed.contains(&root), "unchanged root must be watched");
        assert!(
            installed.contains(&new_directory),
            "new watches must survive"
        );
        assert!(
            !installed.contains(&directory),
            "stale helper result installed a watch on a replacement symlink"
        );
        assert!(
            !installed.contains(&child),
            "unchanged child inode below a replacement symlink must not be watched"
        );
    }

    #[test]
    fn completed_walk_rejects_replaced_directory_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let directory = root.join("collected");
        std::fs::create_dir(&directory).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let walked = collect_watch_directories(
            &crate::watcher_filter::WatcherWalkBoundary::for_root(&root).unwrap(),
            &matcher,
            &[],
        );
        std::fs::rename(&directory, root.join("old-directory")).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            desired_watch_directories(walked, &BTreeSet::new(), &matcher, &[], &root),
            BTreeSet::from([root]),
            "a replacement directory must be planned from its own create event"
        );
    }

    #[test]
    fn matcher_symlink_resolution_does_not_block_healthy_events() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir(root.join("healthy")).unwrap();
        let mut builder = GitignoreBuilder::new(&root);
        builder.add_line(None, "*.tmp").unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let (tx, rx) = mpsc::channel();
        let (installed_tx, installed_rx) = mpsc::channel();
        let _watcher = ProjectWatcher::create_with_watch(
            root.clone(),
            Vec::new(),
            tx,
            matcher.clone(),
            Arc::new(AtomicU64::new(1)),
            move |watcher, path| {
                watcher.watch(path, RecursiveMode::NonRecursive)?;
                installed_tx.send(path.to_path_buf()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        let incoming = tempfile::tempdir().unwrap();
        std::fs::create_dir(incoming.path().join("tree")).unwrap();
        let external_target = incoming.path().join("source.rs");
        std::fs::write(&external_target, "external source").unwrap();
        std::os::unix::fs::symlink(&external_target, incoming.path().join("tree/link.tmp"))
            .unwrap();
        // WalkBlock pauses canonicalize_watcher_path for this link to simulate
        // an unavailable drive. Its Drop releases the pause before the watcher
        // joins its backend thread, even when an assertion panics.
        let link = root.join("incoming/link.tmp");
        let block = WalkBlock::new(link.clone());
        std::fs::rename(incoming.path().join("tree"), root.join("incoming")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let installed = installed_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("new directory was not watched");
            if installed == root.join("incoming") {
                break;
            }
        }
        let healthy = root.join("healthy/change.rs");
        std::fs::write(&healthy, "source").unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("healthy folder event stalled behind matcher symlink resolution")
                .unwrap();
            if event.paths.contains(&healthy) {
                break;
            }
        }
        assert_eq!(block.gate.state.lock().unwrap().1, 0);
        assert!(
            watcher_path_is_ignored_by_matcher(&matcher, &link, &root),
            "matcher must use the link's lexical name, not its external target"
        );
    }

    #[test]
    fn new_tree_scans_only_directory_creates_renames_and_changed_boundaries() {
        let path = PathBuf::from("/project/deep/source.rs");
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(notify::event::RemoveKind::File),
            EventKind::Remove(notify::event::RemoveKind::Folder),
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Any)),
        ] {
            let event = Event::new(kind).add_path(path.clone());
            assert!(new_tree_event_paths(&event, &[]).is_empty());
        }
        for kind in [
            EventKind::Create(CreateKind::Folder),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        ] {
            let event = Event::new(kind).add_path(path.clone());
            assert_eq!(
                new_tree_event_paths(&event, &[]),
                BTreeSet::from([path.clone()])
            );
        }
        let removed = Event::new(EventKind::Remove(notify::event::RemoveKind::File));
        assert_eq!(
            new_tree_event_paths(&removed, &[path.clone()]),
            BTreeSet::from([path])
        );
    }

    #[test]
    fn watcher_scans_skip_different_devices_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let mounted = root.join("mounted");
        std::fs::create_dir_all(mounted.join("deep")).unwrap();
        std::fs::create_dir(root.join("local")).unwrap();
        std::os::unix::fs::symlink(&mounted, root.join("linked")).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        crate::watcher_filter::EXCLUSION_TEST_DEVICE
            .with(|device| *device.borrow_mut() = Some(mounted));
        let initial = collect_watch_directories(
            &crate::watcher_filter::WatcherWalkBoundary::for_root(&root).unwrap(),
            &matcher,
            &[],
        );
        let (_, added, _) = walk_new_tree(
            &root,
            &matcher,
            &[],
            100,
            Instant::now() + Duration::from_secs(1),
            |_| {},
        );
        crate::watcher_filter::EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = None);
        let expected = BTreeSet::from([root.clone(), root.join("local")]);
        assert_eq!(initial.into_keys().collect::<BTreeSet<_>>(), expected);
        assert_eq!(added, expected);
    }

    #[test]
    fn new_directory_scan_rejects_a_symlink_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let destination = root.join("real");
        std::fs::create_dir(&destination).unwrap();
        let link = root.join("linked");
        std::os::unix::fs::symlink(&destination, &link).unwrap();
        let mut watched = BTreeSet::new();
        let mut pending = PendingNewDirectoryFiles::default();
        watch_new_tree(
            &link,
            &crate::watcher_filter::WatcherWalkBoundary::for_root(&root).unwrap(),
            &Arc::new(RwLock::new(None)),
            &[],
            &mut watched,
            &mut pending,
            100,
            Instant::now() + Duration::from_secs(1),
            |_| panic!("symlink must not be watched"),
        )
        .unwrap();
        assert!(watched.is_empty());
    }

    #[test]
    fn removed_directory_prunes_descendant_watches() {
        let mut watched = BTreeSet::from([
            PathBuf::from("/root"),
            PathBuf::from("/root/gone"),
            PathBuf::from("/root/gone/nested"),
            PathBuf::from("/root/keep"),
        ]);
        let mut removed = Vec::new();
        forget_removed_watch_paths(&mut watched, &[PathBuf::from("/root/gone")], |path| {
            removed.push(path.to_path_buf())
        });
        assert_eq!(
            watched,
            BTreeSet::from([PathBuf::from("/root"), PathBuf::from("/root/keep")])
        );
        assert_eq!(removed.len(), 2);
    }

    #[test]
    fn rebuild_reinstalls_retained_watch_paths() {
        let previous = BTreeSet::from([
            PathBuf::from("/root/retained"),
            PathBuf::from("/root/dropped"),
        ]);
        let desired = BTreeSet::from([PathBuf::from("/root/retained")]);
        let mut watched = Vec::new();
        let mut unwatched = Vec::new();
        let rebuilt = reconcile_watch_directories(&previous, &desired, |path, install| {
            if install {
                watched.push(path.to_path_buf());
            } else {
                unwatched.push(path.to_path_buf());
            }
            Ok(install)
        })
        .unwrap();
        assert_eq!(rebuilt, desired);
        assert_eq!(watched, vec![PathBuf::from("/root/retained")]);
        assert_eq!(unwatched, vec![PathBuf::from("/root/dropped")]);
    }

    #[test]
    fn watch_root_below_infrastructure_ancestor_is_not_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target/project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/generated")).unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(
            GitignoreBuilder::new(&root).build().unwrap(),
        ))));
        assert_eq!(
            collect_watch_directories(
                &crate::watcher_filter::WatcherWalkBoundary::for_root(&root).unwrap(),
                &matcher,
                &[]
            )
            .into_keys()
            .collect::<BTreeSet<_>>(),
            BTreeSet::from([root.clone(), root.join("src")])
        );
    }

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
    fn issue645_inotify_nested_git_churn_never_reaches_drain() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for name in ["service-a", "service-b"] {
            let repo = root.join(name);
            std::fs::create_dir(&repo).unwrap();
            assert!(std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(&repo)
                .status()
                .unwrap()
                .success());
        }
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            root.clone(),
            Vec::new(),
            tx,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
        )
        .unwrap();
        assert_eq!(
            watcher.watched_directory_count(),
            3,
            "metadata trees must have no inotify watches"
        );
        for name in ["service-a", "service-b"] {
            let churn = root.join(name).join(".git/worktrees/linked");
            std::fs::create_dir_all(&churn).unwrap();
            for index in 0..64 {
                std::fs::write(churn.join(format!("entry-{index}")), "churn").unwrap();
            }
            std::fs::remove_dir_all(churn).unwrap();
        }
        let source = root.join("service-a/source.rs");
        std::fs::write(&source, "fn visible() {}\n").unwrap();
        let mut saw_source = false;
        let deadline = Instant::now() + Duration::from_secs(1);
        while let Ok(event) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            let event = event.unwrap();
            assert!(
                !event
                    .paths
                    .iter()
                    .any(|path| path.components().any(|part| part.as_os_str() == ".git")),
                "git churn reached drain: {event:?}"
            );
            saw_source |= event.paths.contains(&source);
        }
        assert!(saw_source, "positive source-event control must reach drain");
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
    fn symlink_replaced_by_source_directory_reports_later_edits() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let foo = root.join("foo");
        std::os::unix::fs::symlink(external.path(), &foo).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            root.clone(),
            Vec::new(),
            tx,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
        )
        .unwrap();
        assert_eq!(watcher.watched_directory_count(), 1);

        std::fs::remove_file(&foo).unwrap();
        std::fs::create_dir(&foo).unwrap();
        let source = foo.join("source.rs");
        std::fs::write(&source, "pub fn before() {}\n").unwrap();
        wait_for_event(&rx, "replacement directory creation", |event| {
            event.kind == EventKind::Create(CreateKind::Folder) && event.paths.contains(&foo)
        });
        // A write racing the directory's watch installation may already have
        // queued a data event. Drain it before checking the later edit.
        while let Ok(event) = rx.recv_timeout(Duration::from_millis(200)) {
            event.expect("backend should remain available");
        }
        std::fs::write(&source, "pub fn after() {}\n").unwrap();
        wait_for_event(&rx, "later edit inside replacement directory", |event| {
            matches!(event.kind, EventKind::Modify(ModifyKind::Data(_)))
                && event.paths.contains(&source)
        });
        assert_eq!(watcher.watched_directory_count(), 2);
    }

    #[test]
    fn symlink_replaced_by_symlink_stays_excluded_and_unopened() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::create_dir(second.path().join("nested")).unwrap();
        let foo = root.join("foo");
        std::os::unix::fs::symlink(first.path(), &foo).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let selected = crate::watcher_filter::derive_excluded_subtrees(&root, &matcher, None);
        let mut state = crate::watcher_filter::WatcherExclusionPaths::new(&root, &selected);
        assert_eq!(state.paths(), vec![foo.clone()]);
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            root.clone(),
            Vec::new(),
            tx,
            matcher.clone(),
            Arc::new(AtomicU64::new(1)),
        )
        .unwrap();

        std::fs::remove_file(&foo).unwrap();
        std::os::unix::fs::symlink(second.path(), &foo).unwrap();
        assert_external_replacement_unopened(&root, &foo, &matcher, &mut state);
        wait_for_external_replacement(&root, &foo, &rx, &watcher);
    }

    #[test]
    fn source_directory_replaced_by_symlink_becomes_excluded_and_unopened() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir(external.path().join("nested")).unwrap();
        let foo = root.join("foo");
        std::fs::create_dir(&foo).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let selected = crate::watcher_filter::derive_excluded_subtrees(&root, &matcher, None);
        let mut state = crate::watcher_filter::WatcherExclusionPaths::new(&root, &selected);
        assert!(state.paths().is_empty());
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            root.clone(),
            Vec::new(),
            tx,
            matcher.clone(),
            Arc::new(AtomicU64::new(1)),
        )
        .unwrap();
        assert_eq!(watcher.watched_directory_count(), 2);

        std::fs::remove_dir(&foo).unwrap();
        std::os::unix::fs::symlink(external.path(), &foo).unwrap();
        assert_external_replacement_unopened(&root, &foo, &matcher, &mut state);
        wait_for_external_replacement(&root, &foo, &rx, &watcher);
    }

    fn wait_for_external_replacement(
        root: &Path,
        foo: &Path,
        rx: &mpsc::Receiver<notify::Result<Event>>,
        watcher: &ProjectWatcher,
    ) {
        wait_for_event(rx, "replacement symlink creation", |event| {
            matches!(event.kind, EventKind::Create(_)) && event.paths.iter().any(|path| path == foo)
        });
        assert_eq!(watcher.watched_directory_count(), 1);
        assert!(crate::context::watcher_counters_for_root(root)
            .backend_exclusions()
            .paths
            .contains(&foo.to_path_buf()));
    }

    fn assert_external_replacement_unopened(
        root: &Path,
        foo: &Path,
        matcher: &SharedGitignore,
        state: &mut crate::watcher_filter::WatcherExclusionPaths,
    ) {
        crate::watcher_filter::EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        state.refresh_touched(&[foo.to_path_buf()]);
        assert_eq!(state.paths(), vec![foo.to_path_buf()]);
        let initial = collect_watch_directories(state.boundary().unwrap(), matcher, &state.paths())
            .into_keys()
            .collect::<BTreeSet<_>>();
        let (_, added, files) = walk_new_tree(
            root,
            matcher,
            &state.paths(),
            100,
            Instant::now() + Duration::from_secs(1),
            |_| {},
        );
        assert_eq!(initial, BTreeSet::from([root.to_path_buf()]));
        assert_eq!(added, initial);
        assert!(files.files.is_empty());
        crate::watcher_filter::EXCLUSION_TEST_OPENS.with(|opens| {
            let opens = opens.borrow();
            assert!(opens.iter().any(|path| path == root));
            assert!(
                !opens.iter().any(|path| path.starts_with(foo)),
                "replacement symlink must not be opened: {opens:?}"
            );
        });
    }

    fn wait_for_event(
        rx: &mpsc::Receiver<notify::Result<Event>>,
        description: &str,
        matches: impl Fn(&Event) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("never reported: {description}"))
                .expect("backend should remain available");
            if matches(&event) {
                return;
            }
        }
    }

    /// Waits until events have named each of `expected`.
    fn wait_for_paths(rx: &mpsc::Receiver<notify::Result<Event>>, expected: &[PathBuf]) {
        let mut missing = expected.iter().cloned().collect::<BTreeSet<_>>();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !missing.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = rx
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("never reported: {missing:?}"))
                .expect("backend should remain available");
            for path in &event.paths {
                missing.remove(path);
            }
        }
    }

    /// A directory created and filled straight away, before its watch can be
    /// added, still has its files reported: a checkout or a code generator
    /// does exactly this. Nested new directories are covered.
    ///
    /// Ignored files are not checked here. The backend forwards the kernel's
    /// own events unfiltered (the watcher filter downstream applies the ignore
    /// rules), so a write that lands after a watch is installed can name an
    /// ignored file legitimately. That the listing itself skips ignored files
    /// is checked by `new_directory_listing_skips_ignored_files`.
    #[test]
    fn files_written_into_a_new_directory_before_its_watch_are_reported() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "*.log\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let (tx, rx) = mpsc::channel();
        let _watcher = ProjectWatcher::create(
            canonical_root.clone(),
            Vec::new(),
            tx,
            matcher,
            Arc::new(AtomicU64::new(1)),
        )
        .unwrap();

        let fresh = canonical_root.join("fresh");
        std::fs::create_dir_all(fresh.join("nested/deeper")).unwrap();
        let top = fresh.join("top.rs");
        let deep = fresh.join("nested/deeper/deep.rs");
        std::fs::write(&top, "pub fn top() {}\n").unwrap();
        std::fs::write(&deep, "pub fn deep() {}\n").unwrap();
        std::fs::write(fresh.join("build.log"), "noise\n").unwrap();

        wait_for_paths(&rx, &[top, deep]);
    }

    /// Runs [`watch_new_tree`] for `root` with a watch that always succeeds
    /// and calls `on_watch` with each directory as it is watched.
    fn walk_new_tree(
        root: &Path,
        matcher: &SharedGitignore,
        exclusions: &[PathBuf],
        cap: usize,
        deadline: Instant,
        mut on_watch: impl FnMut(&Path),
    ) -> (
        NewDirectoryScan,
        BTreeSet<PathBuf>,
        PendingNewDirectoryFiles,
    ) {
        let mut watched = BTreeSet::new();
        let mut pending = PendingNewDirectoryFiles::default();
        let scan = watch_new_tree(
            root,
            &crate::watcher_filter::WatcherWalkBoundary::for_root(root).unwrap(),
            matcher,
            exclusions,
            &mut watched,
            &mut pending,
            cap,
            deadline,
            |directory| {
                on_watch(directory);
                Ok(true)
            },
        )
        .expect("walk new tree");
        (scan, watched, pending)
    }

    #[test]
    fn new_directory_listing_skips_ignored_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "*.log\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let fresh = canonical_root.join("fresh");
        let excluded = fresh.join("excluded");
        std::fs::create_dir_all(&excluded).unwrap();
        let kept = fresh.join("kept.rs");
        std::fs::write(&kept, "").unwrap();
        std::fs::write(fresh.join("build.log"), "").unwrap();
        std::fs::write(excluded.join("skipped.rs"), "").unwrap();

        let (scan, watched, mut pending) = walk_new_tree(
            &fresh,
            &matcher,
            std::slice::from_ref(&excluded),
            NEW_DIRECTORY_FILE_CAP,
            Instant::now() + Duration::from_secs(5),
            |_| {},
        );
        assert_eq!(scan, NewDirectoryScan::Complete);
        assert_eq!(watched, BTreeSet::from([fresh]));
        assert_eq!(pending.take_event().paths, vec![kept]);
    }

    /// A subdirectory created after its parent's watch is installed but
    /// before the parent is read must still be watched and listed. Reading
    /// first and watching afterwards leaves it neither seen by the read nor
    /// announced by the kernel.
    #[test]
    fn new_tree_watches_each_directory_before_reading_it() {
        let root = tempfile::tempdir().unwrap();
        let fresh = std::fs::canonicalize(root.path()).unwrap().join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        let late_dir = fresh.join("late");
        let late_file = late_dir.join("late.rs");
        let (scan, watched, mut pending) = walk_new_tree(
            &fresh,
            &Arc::new(RwLock::new(None)),
            &[],
            NEW_DIRECTORY_FILE_CAP,
            Instant::now() + Duration::from_secs(5),
            |directory| {
                // Lands between this directory's watch and its read.
                if directory == fresh {
                    std::fs::create_dir(&late_dir).unwrap();
                    std::fs::write(&late_file, "").unwrap();
                }
            },
        );
        assert_eq!(scan, NewDirectoryScan::Complete);
        assert!(watched.contains(&late_dir), "{watched:?}");
        assert_eq!(pending.take_event().paths, vec![late_file]);
    }

    #[test]
    fn new_directory_listing_stops_at_its_file_cap_and_deadline() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir(canonical.join("nested")).unwrap();
        for index in 0..3 {
            std::fs::write(canonical.join(format!("{index}.rs")), "").unwrap();
        }
        let matcher: SharedGitignore = Arc::new(RwLock::new(None));
        let (scan, watched, _) = walk_new_tree(
            &canonical,
            &matcher,
            &[],
            2,
            Instant::now() + Duration::from_secs(5),
            |_| {},
        );
        assert_eq!(scan, NewDirectoryScan::Bounded);
        assert_eq!(
            watched.len(),
            2,
            "bounded listing still watches every directory"
        );
        let (scan, _, _) = walk_new_tree(&canonical, &matcher, &[], 2, Instant::now(), |_| {});
        assert_eq!(
            scan,
            NewDirectoryScan::Bounded,
            "an expired deadline stops the listing"
        );
    }

    #[test]
    fn kernel_events_replace_listed_paths() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let reported = canonical.join("reported.rs");
        let quiet = canonical.join("quiet.rs");
        std::fs::write(&reported, "").unwrap();
        std::fs::write(&quiet, "").unwrap();
        let (scan, _, mut pending) = walk_new_tree(
            &canonical,
            &Arc::new(RwLock::new(None)),
            &[],
            NEW_DIRECTORY_FILE_CAP,
            Instant::now() + Duration::from_secs(5),
            |_| {},
        );
        assert_eq!(scan, NewDirectoryScan::Complete);
        pending.forget(std::slice::from_ref(&reported));
        let event = pending.take_event();
        assert_eq!(event.paths, vec![quiet]);
        assert!(!pending.due());
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
