use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, SendTimeoutError, Sender};
use ignore::gitignore::Gitignore;

pub type SharedGitignore = Arc<RwLock<Option<Arc<Gitignore>>>>;

pub const WATCHER_FLUSH_WINDOW: Duration = Duration::from_millis(250);
pub const WATCHER_MAX_BATCH_PATHS: usize = 1024;
pub const WATCHER_DISPATCH_CHANNEL_CAPACITY: usize = 1024;
const ROOT_DELETED_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const GITIGNORE_REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DISPATCH_SEND_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct WatcherFilterConfig {
    pub project_root: PathBuf,
    pub git_common_dir: Option<PathBuf>,
}

impl WatcherFilterConfig {
    pub fn new(project_root: PathBuf, git_common_dir: Option<PathBuf>) -> Self {
        Self {
            project_root,
            git_common_dir,
        }
    }

    fn git_info_exclude_path(&self) -> PathBuf {
        self.git_common_dir
            .clone()
            .unwrap_or_else(|| self.project_root.join(".git"))
            .join("info")
            .join("exclude")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    KernelDropped,
    UserDropped,
    Unknown,
}

impl RescanReason {
    fn from_event_info(info: Option<&str>) -> Self {
        match info {
            Some("rescan: kernel dropped") => Self::KernelDropped,
            Some("rescan: user dropped") => Self::UserDropped,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::KernelDropped => "kernel_dropped",
            Self::UserDropped => "user_dropped",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherDispatchEvent {
    Paths(Vec<PathBuf>),
    RescanRequired(RescanReason),
    IgnoreRulesChanged { path: PathBuf },
    RootDeleted,
    Error(String),
}

pub struct WatcherThreadHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

/// Result of a bounded watcher-thread join.
pub enum WatcherJoinOutcome {
    Joined,
    TimedOut(JoinHandle<()>),
}

impl WatcherThreadHandle {
    pub fn new(shutdown: Arc<AtomicBool>, join: JoinHandle<()>) -> Self {
        Self {
            shutdown,
            join: Some(join),
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|join| join.is_finished())
    }

    pub fn shutdown_and_join(mut self) {
        self.request_shutdown();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// Request shutdown and wait only up to `timeout` for the watcher thread.
    /// The caller owns the still-live join handle on timeout and can monitor it
    /// without blocking an executor or transport loop.
    pub fn shutdown_and_join_timeout(mut self, timeout: Duration) -> WatcherJoinOutcome {
        self.request_shutdown();
        let Some(join) = self.join.take() else {
            return WatcherJoinOutcome::Joined;
        };
        let deadline = Instant::now() + timeout;
        while !join.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if join.is_finished() {
            let _ = join.join();
            WatcherJoinOutcome::Joined
        } else {
            WatcherJoinOutcome::TimedOut(join)
        }
    }
}

impl Drop for WatcherThreadHandle {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

pub fn watcher_dispatch_channel() -> (Sender<WatcherDispatchEvent>, Receiver<WatcherDispatchEvent>)
{
    crossbeam_channel::bounded(WATCHER_DISPATCH_CHANNEL_CAPACITY)
}

/// Decide whether a `notify::Event` represents a real content change worth
/// invalidating cached state for.
pub fn watcher_event_invalidates(kind: &notify::EventKind) -> bool {
    use notify::event::{MetadataKind, ModifyKind};
    use notify::EventKind;
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(meta)) => !matches!(
            meta,
            MetadataKind::AccessTime
                | MetadataKind::Permissions
                | MetadataKind::Ownership
                | MetadataKind::Extended
        ),
        EventKind::Modify(_) => true,
        _ => false,
    }
}

pub fn watcher_path_is_infra_skip(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, Component::Normal(name) if matches!(
            name.to_str().unwrap_or(""),
            ".git" | ".opencode" | ".alfonso" | ".gsd" | "node_modules" | "target"
        ))
    })
}

/// High-churn ignored directories that can be dropped from the raw event stream
/// before paying for a `realpath` canonicalization.
///
/// A build writes hundreds of thousands of files under `target/` (or
/// `node_modules/` for JS installs); FSEvents delivers every one to AFT, and
/// canonicalizing each just to drop it later in the filter pegs the
/// single-threaded watcher loop. This is a pure path-component scan (no syscall),
/// so the flood is rejected almost for free.
///
/// This deliberately omits `.git`: `.git/info/exclude` changes the corpus ignore
/// set, and dropping `.git` here would hide them from the ignore-relevance check
/// in the full filter. `.git` churn is small next to `target/`, so it stays on
/// the canonicalizing path.
fn watcher_path_is_high_churn_infra(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, Component::Normal(name) if matches!(
            name.to_str().unwrap_or(""),
            ".opencode" | ".alfonso" | ".gsd" | "node_modules" | "target"
        ))
    })
}

fn watcher_path_is_ignore_file(path: &Path) -> bool {
    path.file_name()
        .map(|n| n == ".gitignore" || n == ".aftignore")
        .unwrap_or(false)
}

fn watcher_same_path(path: &Path, target: &Path) -> bool {
    if path == target {
        return true;
    }

    std::fs::canonicalize(target)
        .map(|target| path == target)
        .unwrap_or(false)
}

fn watcher_path_is_git_info_exclude(config: &WatcherFilterConfig, path: &Path) -> bool {
    watcher_same_path(path, &config.git_info_exclude_path())
}

fn watcher_path_is_global_gitignore(path: &Path) -> bool {
    ignore::gitignore::gitconfig_excludes_path()
        .as_deref()
        .is_some_and(|global_ignore| watcher_same_path(path, global_ignore))
}

fn watcher_path_can_change_corpus_ignore(config: &WatcherFilterConfig, path: &Path) -> bool {
    if watcher_path_is_global_gitignore(path) {
        return true;
    }
    if watcher_path_is_git_info_exclude(config, path) {
        return true;
    }
    if !path.starts_with(&config.project_root) {
        return false;
    }

    watcher_path_is_ignore_file(path) && !watcher_path_is_infra_skip(path)
}

pub fn canonicalize_watcher_path(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }

    let parent = path.parent().map(Path::to_path_buf);
    let file_name = path.file_name().map(std::ffi::OsStr::to_os_string);
    match (parent, file_name) {
        (Some(parent), Some(file_name)) => std::fs::canonicalize(parent)
            .map(|canonical_parent| canonical_parent.join(file_name))
            .unwrap_or(path),
        _ => path,
    }
}

fn watcher_path_is_ignored_by_matcher(matcher: &SharedGitignore, path: &Path) -> bool {
    if watcher_path_is_infra_skip(path) {
        return true;
    }

    let guard = matcher
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(matcher) = guard.as_ref() {
        if path.starts_with(matcher.path()) {
            let is_dir = path.is_dir();
            return matcher
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore();
        }
    }

    false
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilteredWatcherPaths {
    pub changed: BTreeSet<PathBuf>,
    pub ignore_file_changed: bool,
}

fn filter_canonical_paths(
    config: &WatcherFilterConfig,
    matcher: &SharedGitignore,
    raw_paths: BTreeSet<PathBuf>,
) -> FilteredWatcherPaths {
    let ignore_file_changed = raw_paths
        .iter()
        .any(|path| watcher_path_can_change_corpus_ignore(config, path));

    let changed = raw_paths
        .into_iter()
        .filter(|path| {
            if watcher_path_is_infra_skip(path) {
                return false;
            }

            if watcher_path_is_global_gitignore(path)
                || watcher_path_is_git_info_exclude(config, path)
            {
                return false;
            }

            if watcher_path_is_ignored_by_matcher(matcher, path) {
                return false;
            }
            true
        })
        .collect();

    FilteredWatcherPaths {
        changed,
        ignore_file_changed,
    }
}

pub fn filter_watcher_raw_paths_for_test<I>(
    config: &WatcherFilterConfig,
    matcher: &SharedGitignore,
    raw_paths: I,
) -> FilteredWatcherPaths
where
    I: IntoIterator<Item = PathBuf>,
{
    let raw_paths = raw_paths
        .into_iter()
        .map(canonicalize_watcher_path)
        .collect::<BTreeSet<_>>();
    filter_canonical_paths(config, matcher, raw_paths)
}

pub fn run_watcher_thread<W, E, F>(
    config: WatcherFilterConfig,
    extra_watch_paths: Vec<PathBuf>,
    matcher: SharedGitignore,
    matcher_generation: Arc<AtomicU64>,
    dispatch_tx: Sender<WatcherDispatchEvent>,
    shutdown: Arc<AtomicBool>,
    attach: F,
) where
    W: Send + 'static,
    E: std::fmt::Display,
    F: FnOnce(PathBuf, Vec<PathBuf>, mpsc::Sender<notify::Result<notify::Event>>) -> Result<W, E>,
{
    let (raw_tx, raw_rx) = mpsc::channel();
    let root_path = config.project_root.clone();
    match attach(root_path.clone(), extra_watch_paths, raw_tx) {
        Ok(_watcher) => {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            crate::slog_info!("watcher started: {}", root_path.display());
            let mut filter = WatcherFilterThread::new(
                config,
                matcher,
                matcher_generation,
                dispatch_tx,
                shutdown,
            );
            filter.run(raw_rx);
        }
        Err(error) => {
            if !shutdown.load(Ordering::SeqCst) {
                log::debug!(
                    "watcher init failed: {} — callers will work with stale data",
                    error
                );
                let _ = dispatch_tx.send(WatcherDispatchEvent::Error(format!(
                    "watcher init failed: {error}"
                )));
            }
        }
    }
}

struct WatcherFilterThread {
    config: WatcherFilterConfig,
    matcher: SharedGitignore,
    matcher_generation: Arc<AtomicU64>,
    dispatch_tx: Sender<WatcherDispatchEvent>,
    shutdown: Arc<AtomicBool>,
    raw_paths: BTreeSet<PathBuf>,
    flush_deadline: Option<Instant>,
}

impl WatcherFilterThread {
    fn new(
        config: WatcherFilterConfig,
        matcher: SharedGitignore,
        matcher_generation: Arc<AtomicU64>,
        dispatch_tx: Sender<WatcherDispatchEvent>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            config,
            matcher,
            matcher_generation,
            dispatch_tx,
            shutdown,
            raw_paths: BTreeSet::new(),
            flush_deadline: None,
        }
    }

    fn run(&mut self, raw_rx: mpsc::Receiver<notify::Result<notify::Event>>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                self.flush_pending();
                return;
            }
            if self.project_root_was_deleted() {
                self.raw_paths.clear();
                let _ = self.send_dispatch(WatcherDispatchEvent::RootDeleted);
                return;
            }
            if self.flush_deadline_reached() {
                if !self.flush_pending() {
                    return;
                }
                continue;
            }

            match raw_rx.recv_timeout(self.next_recv_timeout()) {
                Ok(Ok(event)) => {
                    if event.need_rescan() {
                        let reason = RescanReason::from_event_info(event.info());
                        self.raw_paths.clear();
                        self.flush_deadline = None;
                        if !self.send_dispatch(WatcherDispatchEvent::RescanRequired(reason)) {
                            return;
                        }
                        continue;
                    }
                    if watcher_event_invalidates(&event.kind) && !self.push_raw_paths(event.paths) {
                        return;
                    }
                }
                Ok(Err(error)) => {
                    let _ = self.send_dispatch(WatcherDispatchEvent::Error(error.to_string()));
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.flush_pending() {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !self.shutdown.load(Ordering::SeqCst) {
                        let _ = self.send_dispatch(WatcherDispatchEvent::Error(
                            "watcher channel disconnected".to_string(),
                        ));
                    }
                    return;
                }
            }
        }
    }

    fn project_root_was_deleted(&self) -> bool {
        !self.config.project_root.exists()
    }

    fn push_raw_paths(&mut self, paths: Vec<PathBuf>) -> bool {
        for path in paths {
            // Drop high-churn ignored dirs (target/, node_modules/, agent infra)
            // on the RAW path before canonicalizing. `canonicalize_watcher_path`
            // is a realpath syscall; a build floods FSEvents with hundreds of
            // thousands of target/ paths, and paying a syscall per path only to
            // discard them later pegged this single watcher thread. The full
            // filter still drops these (watcher_path_is_infra_skip), so this is a
            // pure perf short-circuit with no behavior change.
            if watcher_path_is_high_churn_infra(&path) {
                continue;
            }
            // Canonicalize at intake so the set keys (and downstream consumers)
            // see normalized paths — this is what collapses macOS /var ->
            // /private/var aliasing and matches the callgraph/semantic/search
            // cache keys. Same-file repeats within the window still dedup here;
            // the high-churn flood is already dropped above, before this syscall.
            self.raw_paths.insert(canonicalize_watcher_path(path));
        }
        if !self.raw_paths.is_empty() && self.flush_deadline.is_none() {
            self.flush_deadline = Some(Instant::now() + WATCHER_FLUSH_WINDOW);
        }
        if self.raw_paths.len() >= WATCHER_MAX_BATCH_PATHS {
            return self.flush_pending();
        }
        true
    }

    fn next_recv_timeout(&self) -> Duration {
        let root_check = ROOT_DELETED_CHECK_INTERVAL;
        match self.flush_deadline {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .min(root_check),
            None => root_check,
        }
    }

    fn flush_deadline_reached(&self) -> bool {
        self.flush_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn flush_pending(&mut self) -> bool {
        if self.raw_paths.is_empty() {
            self.flush_deadline = None;
            return true;
        }

        let raw_paths = std::mem::take(&mut self.raw_paths);
        self.flush_deadline = None;
        let ignore_path = raw_paths
            .iter()
            .find(|path| watcher_path_can_change_corpus_ignore(&self.config, path))
            .cloned();
        let ignore_file_changed = ignore_path.is_some();
        if let Some(path) = ignore_path {
            let observed_generation = self.matcher_generation.load(Ordering::SeqCst);
            if !self.send_dispatch(WatcherDispatchEvent::IgnoreRulesChanged { path }) {
                return false;
            }
            if !self.wait_for_gitignore_rebuild(observed_generation) {
                return false;
            }
        }

        let filtered = filter_canonical_paths(&self.config, &self.matcher, raw_paths);
        debug_assert_eq!(filtered.ignore_file_changed, ignore_file_changed);
        if filtered.changed.is_empty() {
            return true;
        }
        self.send_dispatch(WatcherDispatchEvent::Paths(
            filtered.changed.into_iter().collect(),
        ))
    }

    fn wait_for_gitignore_rebuild(&self, observed_generation: u64) -> bool {
        while !self.shutdown.load(Ordering::SeqCst)
            && self.matcher_generation.load(Ordering::SeqCst) == observed_generation
        {
            if self.project_root_was_deleted() {
                let _ = self.send_dispatch(WatcherDispatchEvent::RootDeleted);
                return false;
            }
            thread::sleep(GITIGNORE_REBUILD_POLL_INTERVAL);
        }
        !self.shutdown.load(Ordering::SeqCst)
    }

    fn send_dispatch(&self, event: WatcherDispatchEvent) -> bool {
        let mut event = event;
        loop {
            match self
                .dispatch_tx
                .send_timeout(event, DISPATCH_SEND_POLL_INTERVAL)
            {
                Ok(()) => return true,
                Err(SendTimeoutError::Timeout(returned)) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        return false;
                    }
                    event = returned;
                }
                Err(SendTimeoutError::Disconnected(_)) => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ignore::gitignore::GitignoreBuilder;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind,
    };
    use notify::EventKind;
    use tempfile::TempDir;

    fn shared_matcher(root: &Path) -> SharedGitignore {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut builder = GitignoreBuilder::new(&root);
        let ignore = root.join(".gitignore");
        if ignore.exists() {
            if let Some(error) = builder.add(&ignore) {
                panic!("gitignore parse error: {error}");
            }
        }
        let matcher = builder.build().unwrap();
        let matcher = (matcher.num_ignores() > 0).then(|| Arc::new(matcher));
        Arc::new(RwLock::new(matcher))
    }

    #[test]
    fn event_kind_filter_accepts_content_changes_only() {
        assert!(watcher_event_invalidates(&EventKind::Create(
            CreateKind::File
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Data(DataChange::Content)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::AccessTime)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Permissions)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Open(AccessMode::Read)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Other));
    }

    #[test]
    fn high_churn_infra_skip_drops_build_dirs_but_keeps_git_and_source() {
        // target/ and node_modules/ are dropped on the raw path before the
        // realpath syscall — this is the build-flood short-circuit.
        assert!(watcher_path_is_high_churn_infra(Path::new(
            "/proj/target/debug/deps/foo.o"
        )));
        assert!(watcher_path_is_high_churn_infra(Path::new(
            "/proj/node_modules/.bin/x"
        )));
        assert!(watcher_path_is_high_churn_infra(Path::new(
            "/proj/.alfonso/notes/x"
        )));
        // .git is deliberately NOT high-churn-skipped: .git/info/exclude must
        // still reach the ignore-relevance check.
        assert!(!watcher_path_is_high_churn_infra(Path::new(
            "/proj/.git/info/exclude"
        )));
        // Source files always pass through to canonicalization + filtering.
        assert!(!watcher_path_is_high_churn_infra(Path::new(
            "/proj/src/main.rs"
        )));
        // The full filter still drops .git (and everything high-churn does).
        assert!(watcher_path_is_infra_skip(Path::new("/proj/.git/index")));
    }

    #[test]
    fn rescan_event_dispatches_control_and_supersedes_pending_paths() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let pending = root.join("pending.rs");
        std::fs::write(&pending, "fn main() {}\n").unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let config = WatcherFilterConfig::new(root, None);
        let mut filter = WatcherFilterThread::new(
            config,
            matcher,
            generation,
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let handle = thread::spawn(move || filter.run(raw_rx));

        let mut granular = notify::Event::new(EventKind::Create(CreateKind::File));
        granular.paths.push(pending);
        raw_tx.send(Ok(granular)).unwrap();
        for (info, expected) in [
            (Some("rescan: kernel dropped"), RescanReason::KernelDropped),
            (Some("rescan: user dropped"), RescanReason::UserDropped),
            (None, RescanReason::Unknown),
        ] {
            let mut event = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
            if let Some(info) = info {
                event = event.set_info(info);
            }
            raw_tx.send(Ok(event)).unwrap();
            assert_eq!(
                dispatch_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("rescan event"),
                WatcherDispatchEvent::RescanRequired(expected)
            );
        }
        assert!(
            dispatch_rx
                .recv_timeout(WATCHER_FLUSH_WINDOW + Duration::from_millis(100))
                .is_err(),
            "pending granular paths should be cleared by a rescan signal"
        );

        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();
    }

    #[test]
    fn filters_gitignored_paths_with_shared_matcher() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored/file.ts"), "ignored").unwrap();
        std::fs::write(root.join("kept.ts"), "kept").unwrap();
        let matcher = shared_matcher(&root);
        let config = WatcherFilterConfig::new(root.clone(), None);

        let filtered = filter_watcher_raw_paths_for_test(
            &config,
            &matcher,
            [root.join("ignored/file.ts"), root.join("kept.ts")],
        );

        assert!(!filtered.changed.contains(&root.join("ignored/file.ts")));
        assert!(filtered.changed.contains(&root.join("kept.ts")));
    }

    #[test]
    fn ignore_rule_paths_are_control_only_for_external_excludes() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let git_info = root.join(".git").join("info");
        std::fs::create_dir_all(&git_info).unwrap();
        let exclude = git_info.join("exclude");
        std::fs::write(&exclude, "ignored/\n").unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let config = WatcherFilterConfig::new(root, None);

        let filtered = filter_watcher_raw_paths_for_test(&config, &matcher, [exclude]);

        assert!(filtered.ignore_file_changed);
        assert!(filtered.changed.is_empty());
    }

    #[test]
    fn root_deleted_sends_control_and_exits() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let config = WatcherFilterConfig::new(root.clone(), None);
        let mut filter = WatcherFilterThread::new(
            config,
            matcher,
            generation,
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let handle = thread::spawn(move || filter.run(raw_rx));
        let _raw_tx = raw_tx;
        std::fs::remove_dir_all(&root).unwrap();

        let event = dispatch_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("root deleted event");
        assert_eq!(event, WatcherDispatchEvent::RootDeleted);
        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap();
    }
}
