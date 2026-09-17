use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, SendTimeoutError, Sender};
use ignore::gitignore::Gitignore;

pub type SharedGitignore = Arc<RwLock<Option<Arc<Gitignore>>>>;

pub const WATCHER_FLUSH_WINDOW: Duration = Duration::from_millis(250);
pub const WATCHER_MAX_BATCH_PATHS: usize = 1024;
pub const WATCHER_DISPATCH_CHANNEL_CAPACITY: usize = 1024;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) const WATCHER_EXCLUSION_LIMIT: usize = 8;
const ROOT_DELETED_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const GITIGNORE_REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DISPATCH_SEND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WATCHER_ATTRIBUTION_RING_CAPACITY: usize = 512;
const WATCHER_OVERFLOW_PREFIX_LIMIT: usize = 5;
const WATCHER_OBSERVED_EXCLUSION_LIMIT: usize = 32;

#[derive(Debug, Clone)]
pub struct WatcherFilterConfig {
    pub project_root: PathBuf,
    pub git_common_dir: Option<PathBuf>,
    counters: Arc<crate::context::WatcherCounters>,
}

impl WatcherFilterConfig {
    pub fn new(project_root: PathBuf, git_common_dir: Option<PathBuf>) -> Self {
        let counters = crate::context::watcher_counters_for_root(&project_root);
        Self {
            project_root,
            git_common_dir,
            counters,
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
    BufferOverflow,
    KernelDropped,
    UserDropped,
    Unknown,
}

impl RescanReason {
    fn from_event_info(info: Option<&str>) -> Self {
        match info {
            Some("rescan: buffer overflow") => Self::BufferOverflow,
            Some("rescan: kernel dropped") => Self::KernelDropped,
            Some("rescan: user dropped") => Self::UserDropped,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BufferOverflow => "buffer_overflow",
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

fn watcher_path_is_git_head_metadata(config: &WatcherFilterConfig, path: &Path) -> bool {
    crate::alias::capture_git_head_metadata(&config.project_root, config.git_common_dir.as_deref())
        .is_ok_and(|metadata| metadata.matches_path(path))
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

pub(crate) fn watcher_path_is_ignored_by_matcher(matcher: &SharedGitignore, path: &Path) -> bool {
    if watcher_path_is_infra_skip(path) {
        return true;
    }

    let guard = matcher
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    watcher_path_is_ignored(guard.as_deref(), path)
}

fn watcher_path_is_ignored(matcher: Option<&Gitignore>, path: &Path) -> bool {
    matcher.is_some_and(|matcher| {
        path.starts_with(matcher.path())
            && matcher
                .matched_path_or_any_parents(path, path.is_dir())
                .is_ignore()
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
const WATCHER_EXCLUSION_SEEDS: [&str; 17] = [
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".turbo",
    ".cache",
    "coverage",
    "out",
    ".gradle",
    ".dart_tool",
    "Pods",
    "DerivedData",
];

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherExclusionSource {
    Seed,
    Ranked,
    Gitignore,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl WatcherExclusionSource {
    fn priority(self) -> u8 {
        match self {
            Self::Seed => 0,
            Self::Ranked => 1,
            Self::Gitignore => 2,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Ranked => "ranked",
            Self::Gitignore => "gitignore",
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatcherExclusion {
    path: PathBuf,
    source: WatcherExclusionSource,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl WatcherExclusion {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn source(&self) -> WatcherExclusionSource {
        self.source
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn watcher_exclusion_paths(exclusions: &[WatcherExclusion]) -> Vec<PathBuf> {
    exclusions
        .iter()
        .map(|exclusion| exclusion.path.clone())
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GitignoreOrder {
    source_priority: u8,
    source_path: PathBuf,
    line: usize,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl Default for GitignoreOrder {
    fn default() -> Self {
        Self {
            source_priority: u8::MAX,
            source_path: PathBuf::new(),
            line: usize::MAX,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl GitignoreOrder {
    fn for_glob(root: &Path, glob: &ignore::gitignore::Glob) -> Self {
        let Some(source) = glob.from() else {
            return Self::default();
        };
        let source_path = source
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| source.to_path_buf());
        let source_priority = if !source.starts_with(root) {
            0
        } else if source == root.join(".gitignore") {
            1
        } else if source == root.join(".aftignore") {
            2
        } else if source.ends_with(Path::new("info/exclude")) {
            3
        } else {
            4
        };
        let line = fs::read_to_string(source)
            .ok()
            .and_then(|contents| {
                contents.lines().position(|line| {
                    let normalized = if line.ends_with("\\ ") {
                        line
                    } else {
                        line.trim_end()
                    };
                    normalized == glob.original()
                })
            })
            .unwrap_or(usize::MAX);
        Self {
            source_priority,
            source_path,
            line,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn exclusion_seed_priority(relative: &Path, is_git: bool, is_ignored: bool) -> Option<usize> {
    let mut components = relative.components();
    let Component::Normal(name) = components.next()? else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    let priority = WATCHER_EXCLUSION_SEEDS
        .iter()
        .position(|candidate| name == std::ffi::OsStr::new(candidate))?;
    (is_git || is_ignored).then_some(priority)
}

/// Choose ignored directory boundaries that an OS watcher can omit entirely.
///
/// The eight backend slots are filled in three passes: existing root-level
/// directories with a known high event volume, prefixes ranked by a previous
/// overflow, then the remaining ignored boundaries in gitignore order. `.git`
/// is eligible for the first seed slot only when it is a directory; linked
/// worktrees use a `.git` file and must keep watching their root normally.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn derive_excluded_subtrees(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
) -> Vec<WatcherExclusion> {
    #[derive(Debug)]
    struct Candidate {
        path: PathBuf,
        source: WatcherExclusionSource,
        seed_priority: usize,
        observed_count: u64,
        gitignore_order: GitignoreOrder,
    }

    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let matcher = matcher
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let observed = crate::context::watcher_counters_for_root(&root)
        .observed_exclusion_prefixes()
        .into_iter()
        .map(|prefix| (PathBuf::from(prefix.prefix), prefix.count))
        .collect::<BTreeMap<_, _>>();
    let root_git = root.join(".git");
    let mut candidates = Vec::<Candidate>::new();
    let mut stack = vec![root.clone()];

    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                continue;
            }
            let is_git = path == root_git;
            let matched_glob = matcher.as_deref().and_then(|matcher| {
                match matcher.matched_path_or_any_parents(&path, true) {
                    ignore::Match::Ignore(glob) => Some(glob),
                    ignore::Match::None | ignore::Match::Whitelist(_) => None,
                }
            });
            let is_ignored = matched_glob.is_some();
            if is_git || is_ignored {
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                let observed_count = observed.get(relative).copied();
                let seed_priority = exclusion_seed_priority(relative, is_git, is_ignored);
                let source = if seed_priority.is_some() {
                    WatcherExclusionSource::Seed
                } else if observed_count.is_some() {
                    WatcherExclusionSource::Ranked
                } else {
                    WatcherExclusionSource::Gitignore
                };
                candidates.push(Candidate {
                    path,
                    source,
                    seed_priority: seed_priority.unwrap_or(usize::MAX),
                    observed_count: observed_count.unwrap_or_default(),
                    gitignore_order: matched_glob
                        .map(|glob| GitignoreOrder::for_glob(&root, glob))
                        .unwrap_or_default(),
                });
            } else {
                stack.push(path);
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.source
            .priority()
            .cmp(&right.source.priority())
            .then_with(|| match left.source {
                WatcherExclusionSource::Seed => {
                    left.seed_priority.cmp(&right.seed_priority)
                }
                WatcherExclusionSource::Ranked => right
                    .observed_count
                    .cmp(&left.observed_count)
                    .then_with(|| left.path.cmp(&right.path)),
                WatcherExclusionSource::Gitignore => left
                    .gitignore_order
                    .cmp(&right.gitignore_order)
                    .then_with(|| left.path.cmp(&right.path)),
            })
    });
    let mut exclusions = candidates
        .into_iter()
        .map(|candidate| WatcherExclusion {
            path: candidate.path,
            source: candidate.source,
        })
        .collect::<Vec<_>>();
    if let Some(max_paths) = max_paths {
        exclusions.truncate(max_paths);
    }
    exclusions
}

const WATCHER_OBSERVATION_STATE_PREFIX: &str = "watcher.observed_exclusion_prefixes";

fn watcher_observation_state_key(root: &Path) -> String {
    format!(
        "{WATCHER_OBSERVATION_STATE_PREFIX}:{}",
        crate::path_identity::project_scope_key(root)
    )
}

fn valid_observed_exclusion_prefix(prefix: &crate::context::WatcherOverflowPrefix) -> bool {
    prefix.count > 0
        && !prefix.prefix.is_empty()
        && Path::new(&prefix.prefix)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

pub(crate) fn load_watcher_observations(
    root: &Path,
    counters: &crate::context::WatcherCounters,
    db: Option<&Arc<Mutex<crate::db::TrackedConnection>>>,
) {
    let Some(db) = db else {
        return;
    };
    let conn = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(Some(raw)) =
        crate::db::state::get_host_state(&conn, &watcher_observation_state_key(root))
    else {
        return;
    };
    let Ok(mut prefixes) = serde_json::from_str::<Vec<crate::context::WatcherOverflowPrefix>>(&raw)
    else {
        return;
    };
    prefixes.retain(valid_observed_exclusion_prefix);
    prefixes.truncate(WATCHER_OBSERVED_EXCLUSION_LIMIT);
    counters.set_observed_exclusion_prefixes(prefixes);
}

pub(crate) fn persist_watcher_observations(
    root: &Path,
    counters: &crate::context::WatcherCounters,
    db: Option<&Arc<Mutex<crate::db::TrackedConnection>>>,
) {
    let Some(db) = db else {
        return;
    };
    let prefixes = counters.observed_exclusion_prefixes();
    let Ok(value) = serde_json::to_string(&prefixes) else {
        return;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let conn = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(error) = crate::db::state::set_host_state(
        &conn,
        &watcher_observation_state_key(root),
        &value,
        now_ms,
    ) {
        crate::slog_warn!(
            "failed to persist watcher overflow prefixes for {}: {}",
            root.display(),
            error
        );
    }
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
            if watcher_path_is_git_head_metadata(config, path) {
                return true;
            }
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
    recent_paths: VecDeque<(PathBuf, Instant)>,
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
            recent_paths: VecDeque::with_capacity(WATCHER_ATTRIBUTION_RING_CAPACITY),
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
                    self.config.counters.note_raw_event();
                    if event.need_rescan() {
                        let reason = RescanReason::from_event_info(event.info());
                        let during_rescan = self.log_overflow(reason);
                        self.raw_paths.clear();
                        self.flush_deadline = None;
                        if !during_rescan
                            && !self.send_dispatch(WatcherDispatchEvent::RescanRequired(reason))
                        {
                            return;
                        }
                        continue;
                    }
                    self.record_recent_paths(&event.paths);
                    if watcher_event_invalidates(&event.kind) {
                        self.config.counters.note_invalidating_event();
                        if !self.push_raw_paths(event.paths) {
                            return;
                        }
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

    fn record_recent_paths(&mut self, paths: &[PathBuf]) {
        let arrived_at = Instant::now();
        for path in paths {
            let relative = path
                .strip_prefix(&self.config.project_root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| {
                    PathBuf::from("<external>")
                        .join(path.file_name().unwrap_or_else(|| path.as_os_str()))
                });
            if self.recent_paths.len() == WATCHER_ATTRIBUTION_RING_CAPACITY {
                self.recent_paths.pop_front();
            }
            self.recent_paths.push_back((relative, arrived_at));
        }
    }

    fn overflow_prefixes(&self) -> Vec<crate::context::WatcherOverflowPrefix> {
        let mut counts = BTreeMap::<String, u64>::new();
        for (path, _) in &self.recent_paths {
            // Root-relative, first two components, joined with `/` on every
            // platform: the prefix is rendered in the overflow log line (one
            // grammar for the fleet's log readers) and persisted for slot
            // ranking, and `Path::new` on Windows reads `/` back as a
            // separator when the prefix is turned into an exclusion path.
            let prefix = path
                .components()
                .filter_map(|component| match component {
                    Component::Normal(name) => Some(name.to_string_lossy()),
                    _ => None,
                })
                .take(2)
                .collect::<Vec<_>>()
                .join("/");
            if prefix.is_empty() {
                continue;
            }
            *counts.entry(prefix).or_default() += 1;
        }
        let mut prefixes = counts
            .into_iter()
            .map(|(prefix, count)| crate::context::WatcherOverflowPrefix { prefix, count })
            .collect::<Vec<_>>();
        prefixes.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.prefix.cmp(&right.prefix))
        });
        prefixes.truncate(WATCHER_OVERFLOW_PREFIX_LIMIT);
        prefixes
    }

    fn observed_exclusion_prefixes(&self) -> Vec<crate::context::WatcherOverflowPrefix> {
        let matcher = self
            .matcher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let root_git = self.config.project_root.join(".git");
        let mut counts = BTreeMap::<String, u64>::new();
        for (path, _) in &self.recent_paths {
            let mut relative = PathBuf::new();
            let mut absolute = self.config.project_root.clone();
            for component in path.components() {
                let Component::Normal(name) = component else {
                    continue;
                };
                relative.push(name);
                absolute.push(name);
                if absolute == root_git || watcher_path_is_ignored(matcher.as_deref(), &absolute) {
                    *counts
                        .entry(relative.to_string_lossy().into_owned())
                        .or_default() += 1;
                    break;
                }
            }
        }
        let mut prefixes = counts
            .into_iter()
            .map(|(prefix, count)| crate::context::WatcherOverflowPrefix { prefix, count })
            .collect::<Vec<_>>();
        prefixes.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.prefix.cmp(&right.prefix))
        });
        prefixes.truncate(WATCHER_OBSERVED_EXCLUSION_LIMIT);
        prefixes
    }

    fn log_overflow(&self, reason: RescanReason) -> bool {
        let prefixes = self.overflow_prefixes();
        self.config
            .counters
            .set_observed_exclusion_prefixes(self.observed_exclusion_prefixes());
        let during_rescan = self.config.counters.note_overflow(reason, prefixes.clone());
        let backend = self.config.counters.backend_exclusions();
        let exclusions = backend
            .paths
            .iter()
            .map(|path| {
                path.strip_prefix(&self.config.project_root)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(",");
        let prefixes = prefixes
            .iter()
            .map(|prefix| format!("{}:{}", prefix.prefix, prefix.count))
            .collect::<Vec<_>>()
            .join(",");
        let span_ms = self
            .recent_paths
            .front()
            .zip(self.recent_paths.back())
            .map(|((_, first), (_, last))| {
                last.saturating_duration_since(*first)
                    .as_millis()
                    .min(u64::MAX as u128) as u64
            })
            .unwrap_or(0);
        let queue_depth = backend
            .queue_depth
            .map(|depth| depth.to_string())
            .unwrap_or_else(|| "unavailable".to_string());
        let line = format!(
            "watcher overflow: reason={} root={} exclusions=[{}] matcher_generation={} top_prefixes=[{}] ring_span_ms={} queue_depth={} rescan_in_progress={}",
            reason.as_str(),
            self.config.project_root.display(),
            exclusions,
            backend.matcher_generation,
            prefixes,
            span_ms,
            queue_depth,
            during_rescan
        );
        emit_watcher_overflow_log(line);
        during_rescan
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
        self.config
            .counters
            .note_paths_after_gitignore(filtered.changed.len());
        if filtered.changed.is_empty() {
            return true;
        }
        let paths = filtered.changed.into_iter().collect::<Vec<_>>();
        let path_count = paths.len();
        if !self.send_dispatch(WatcherDispatchEvent::Paths(paths)) {
            return false;
        }
        self.config.counters.note_paths_dispatched(path_count);
        true
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

fn emit_watcher_overflow_log(line: String) {
    crate::slog_warn!("{line}");
    #[cfg(test)]
    WATCHER_OVERFLOW_LOGS_FOR_TEST
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(line);
}

#[cfg(test)]
static WATCHER_OVERFLOW_LOGS_FOR_TEST: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn take_watcher_overflow_logs_for_test() -> Vec<String> {
    std::mem::take(
        &mut *WATCHER_OVERFLOW_LOGS_FOR_TEST
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
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
    fn overflow_volume_promotes_deep_ignored_prefix_into_next_exclusion_set() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let fallback = [
            "target",
            "node_modules",
            "dist",
            "build",
            ".next",
            "tmp",
            ".bench",
            "coverage",
            "aaa",
            "bbb",
        ];
        for directory in fallback {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        let hot = root.path().join("packages/opencode-plugin/tmp");
        std::fs::create_dir_all(&hot).unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            format!(
                "{}packages/*/tmp/\n",
                fallback
                    .iter()
                    .map(|directory| format!("{directory}/\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let hot = std::fs::canonicalize(hot).unwrap();
        let matcher = shared_matcher(&canonical_root);
        let generation = Arc::new(AtomicU64::new(4));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = crossbeam_channel::bounded(1);
        let (raw_tx, raw_rx) = mpsc::channel();
        let config = WatcherFilterConfig::new(canonical_root.clone(), None);
        let mut filter = WatcherFilterThread::new(
            config,
            Arc::clone(&matcher),
            generation,
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let handle = thread::spawn(move || filter.run(raw_rx));

        for index in 0..64 {
            raw_tx
                .send(Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                    .add_path(hot.join(format!("host-install-{index}")))))
                .unwrap();
        }
        raw_tx
            .send(Ok(
                notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)
            ))
            .unwrap();
        assert_eq!(
            dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            WatcherDispatchEvent::RescanRequired(RescanReason::Unknown)
        );
        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();

        let exclusions =
            derive_excluded_subtrees(&canonical_root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        assert_eq!(exclusions[0].path(), canonical_root.join(".git"));
        assert_eq!(exclusions.last().unwrap().path(), hot);
        assert_eq!(
            exclusions.last().unwrap().source(),
            WatcherExclusionSource::Ranked
        );
    }

    #[test]
    fn observed_exclusion_ranking_survives_state_database_reload() {
        let root = TempDir::new().unwrap();
        let storage = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let hot = root.path().join("packages/opencode-plugin/tmp");
        std::fs::create_dir_all(&hot).unwrap();
        let fallback = [
            "target",
            "node_modules",
            "dist",
            "build",
            ".next",
            "tmp",
            ".bench",
            "coverage",
        ];
        for directory in fallback {
            std::fs::create_dir(root.path().join(directory)).unwrap();
        }
        std::fs::write(
            root.path().join(".gitignore"),
            format!(
                "{}packages/*/tmp/\n",
                fallback
                    .iter()
                    .map(|directory| format!("{directory}/\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let hot = std::fs::canonicalize(hot).unwrap();
        let matcher = shared_matcher(&canonical_root);
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let db = Arc::new(Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        counters.set_observed_exclusion_prefixes(vec![crate::context::WatcherOverflowPrefix {
            prefix: "packages/opencode-plugin/tmp".to_string(),
            count: 37,
        }]);
        persist_watcher_observations(&canonical_root, &counters, Some(&db));
        counters.set_observed_exclusion_prefixes(Vec::new());

        load_watcher_observations(&canonical_root, &counters, Some(&db));

        assert_eq!(
            counters.observed_exclusion_prefixes(),
            vec![crate::context::WatcherOverflowPrefix {
                prefix: "packages/opencode-plugin/tmp".to_string(),
                count: 37,
            }]
        );
        let exclusions =
            derive_excluded_subtrees(&canonical_root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        assert_eq!(exclusions[0].path(), canonical_root.join(".git"));
        assert_eq!(exclusions.last().unwrap().path(), hot);
        assert_eq!(
            exclusions.last().unwrap().source(),
            WatcherExclusionSource::Ranked
        );
    }

    #[test]
    fn exclusion_derivation_uses_fixed_priority_caps_and_skips_missing_directories() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let priorities = [
            "target",
            "node_modules",
            "dist",
            "build",
            ".next",
            ".venv",
            "venv",
            "__pycache__",
        ];
        for name in priorities {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        std::fs::create_dir(root.path().join("other-generated")).unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            format!(
                "{}other-generated/\nmissing/\n",
                priorities
                    .iter()
                    .rev()
                    .map(|name| format!("{name}/\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let matcher = shared_matcher(root.path());

        let exclusions =
            derive_excluded_subtrees(root.path(), &matcher, Some(WATCHER_EXCLUSION_LIMIT));

        assert_eq!(exclusions.len(), WATCHER_EXCLUSION_LIMIT);
        assert_eq!(
            exclusions[0].path(),
            std::fs::canonicalize(root.path().join(".git")).unwrap()
        );
        assert_eq!(
            watcher_exclusion_paths(&exclusions[1..]),
            priorities[..WATCHER_EXCLUSION_LIMIT - 1]
                .iter()
                .map(|name| std::fs::canonicalize(root.path().join(name)).unwrap())
                .collect::<Vec<_>>()
        );
        assert!(!exclusions
            .iter()
            .any(|exclusion| exclusion.path().ends_with("missing")));
        assert!(!exclusions
            .iter()
            .any(|exclusion| exclusion.path().ends_with("other-generated")));
    }

    #[test]
    fn fresh_root_seeds_heavy_directories_before_late_gitignore_patterns() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let patterns = [
            "generated-01",
            "generated-02",
            "generated-03",
            "generated-04",
            "generated-05",
            "generated-06",
            "generated-07",
            "generated-08",
            "generated-09",
            "build",
            "target",
            "node_modules",
        ];
        for pattern in patterns {
            std::fs::create_dir(root.path().join(pattern)).unwrap();
        }
        std::fs::write(
            root.path().join(".gitignore"),
            patterns
                .iter()
                .map(|pattern| format!("{pattern}/\n"))
                .collect::<String>(),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);

        let exclusions =
            derive_excluded_subtrees(&canonical_root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));

        assert_eq!(exclusions.len(), WATCHER_EXCLUSION_LIMIT);
        assert_eq!(
            watcher_exclusion_paths(&exclusions[..4]),
            [".git", "target", "node_modules", "build"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert!(exclusions[..4]
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Seed));
        assert_eq!(
            watcher_exclusion_paths(&exclusions[4..]),
            patterns[..4]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert!(exclusions[4..]
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Gitignore));

        let source_root = TempDir::new().unwrap();
        std::fs::create_dir(source_root.path().join(".git")).unwrap();
        std::fs::create_dir(source_root.path().join("build")).unwrap();
        std::fs::create_dir(source_root.path().join("ignored")).unwrap();
        std::fs::write(source_root.path().join(".gitignore"), "ignored/\n").unwrap();
        let source_root = std::fs::canonicalize(source_root.path()).unwrap();
        let matcher = shared_matcher(&source_root);
        let exclusions =
            derive_excluded_subtrees(&source_root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        assert!(!exclusions
            .iter()
            .any(|exclusion| exclusion.path() == source_root.join("build")));
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
    fn git_head_and_resolved_ref_bypass_git_infra_filter() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let git = root.join(".git");
        let head = git.join("HEAD");
        let resolved_ref = git.join("refs/heads/main");
        std::fs::create_dir_all(resolved_ref.parent().unwrap()).unwrap();
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();
        std::fs::write(&resolved_ref, "0000000000000000000000000000000000000000\n").unwrap();
        std::fs::write(git.join("index"), []).unwrap();
        let config = WatcherFilterConfig::new(root.clone(), None);
        let matcher = shared_matcher(&root);

        let filtered = filter_watcher_raw_paths_for_test(
            &config,
            &matcher,
            [head.clone(), resolved_ref.clone(), git.join("index")],
        );

        assert_eq!(filtered.changed, BTreeSet::from([head, resolved_ref]));
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
        let counters = Arc::clone(&config.counters);
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
            (
                Some("rescan: buffer overflow"),
                RescanReason::BufferOverflow,
            ),
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
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.raw_events_total, 5);
        assert_eq!(snapshot.invalidating_events_total, 1);
        assert_eq!(snapshot.paths_after_gitignore_total, 0);
        assert_eq!(snapshot.paths_dispatched_total, 0);

        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();
    }

    #[test]
    fn overflow_log_attributes_excluded_and_nonexcluded_burst_prefixes() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let target = root.join("target/cache");
        let source = root.join("src/generated");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let matcher = shared_matcher(&root);
        let generation = Arc::new(AtomicU64::new(7));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = crossbeam_channel::bounded(1);
        let (raw_tx, raw_rx) = mpsc::channel();
        let config = WatcherFilterConfig::new(root.clone(), None);
        config.counters.set_backend_exclusions(
            7,
            (0..WATCHER_EXCLUSION_LIMIT)
                .map(|index| root.join(format!("excluded-{index}")))
                .collect(),
        );
        let counters = Arc::clone(&config.counters);
        let mut filter = WatcherFilterThread::new(
            config,
            matcher,
            generation,
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let handle = thread::spawn(move || filter.run(raw_rx));

        for index in 0..20 {
            raw_tx
                .send(Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                    .add_path(target.join(format!("artifact-{index}")))))
                .unwrap();
        }
        for index in 0..7 {
            raw_tx
                .send(Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                    .add_path(source.join(format!("source-{index}.rs")))))
                .unwrap();
        }
        raw_tx
            .send(Ok(
                notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)
            ))
            .unwrap();
        assert_eq!(
            dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            WatcherDispatchEvent::RescanRequired(RescanReason::Unknown)
        );

        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();

        let lines = take_watcher_overflow_logs_for_test();
        let line = lines
            .iter()
            .find(|line| line.contains(&format!("root={}", root.display())))
            .unwrap_or_else(|| panic!("missing overflow line for {}: {lines:?}", root.display()));
        assert!(line.contains("matcher_generation=7"), "line: {line}");
        assert!(line.contains("target/cache:20"), "line: {line}");
        assert!(line.contains("src/generated:7"), "line: {line}");
        assert!(line.contains("queue_depth=unavailable"), "line: {line}");
        assert!(line.contains("rescan_in_progress=false"), "line: {line}");
        for index in 0..WATCHER_EXCLUSION_LIMIT {
            assert!(line.contains(&format!("excluded-{index}")), "line: {line}");
        }
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.overflows_total, 1);
        assert_eq!(snapshot.overflows_during_rescan, 0);
        assert_eq!(snapshot.last_overflow_prefixes[0].prefix, "target/cache");
        assert_eq!(snapshot.last_overflow_prefixes[0].count, 20);
    }

    #[test]
    fn watcher_thread_records_filter_pipeline_counters() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let changed = root.join("changed.rs");
        std::fs::write(&changed, "fn changed() {}\n").unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let config = WatcherFilterConfig::new(root, None);
        let counters = Arc::clone(&config.counters);
        let mut filter = WatcherFilterThread::new(
            config,
            matcher,
            generation,
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let handle = thread::spawn(move || filter.run(raw_rx));

        let mut event = notify::Event::new(EventKind::Create(CreateKind::File));
        event.paths.push(changed.clone());
        raw_tx.send(Ok(event)).unwrap();
        assert_eq!(
            dispatch_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("filtered paths"),
            WatcherDispatchEvent::Paths(vec![changed])
        );

        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.raw_events_total, 1);
        assert_eq!(snapshot.raw_events_since_last_rescan, 1);
        assert_eq!(snapshot.invalidating_events_total, 1);
        assert_eq!(snapshot.invalidating_events_since_last_rescan, 1);
        assert_eq!(snapshot.paths_after_gitignore_total, 1);
        assert_eq!(snapshot.paths_after_gitignore_since_last_rescan, 1);
        assert_eq!(snapshot.paths_dispatched_total, 1);
        assert_eq!(snapshot.paths_dispatched_since_last_rescan, 1);
    }

    #[test]
    fn configured_context_and_filter_thread_share_root_counters() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config::default(),
        );
        ctx.update_config(|config| config.project_root = Some(root.clone()));
        let config = WatcherFilterConfig::new(root, None);

        config.counters.note_raw_event();

        assert_eq!(ctx.watcher_counters().snapshot().raw_events_total, 1);
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
