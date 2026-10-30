use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(any(test, feature = "test-timing-hooks"))]
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use ignore::gitignore::Gitignore;

pub type SharedGitignore = Arc<RwLock<Option<Arc<Gitignore>>>>;

pub const WATCHER_FLUSH_WINDOW: Duration = Duration::from_millis(250);
pub const WATCHER_MAX_BATCH_PATHS: usize = 1024;
pub const WATCHER_DISPATCH_CHANNEL_CAPACITY: usize = 1024;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) const WATCHER_EXCLUSION_LIMIT: usize = 8;
/// How deep below the root a bare ecosystem name is still enumerated. A
/// gitignore pattern like `node_modules` matches at every depth, but an
/// exclusion is one exact path, so the copies that exist have to be found. The
/// bound keeps that search off the deep end of a monorepo: workspace layouts
/// put the heavy directory at `packages/<name>/node_modules`, which is depth 3.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const WATCHER_NESTED_ECOSYSTEM_MAX_DEPTH: usize = 3;
/// Entries read from one directory's top level when sizing it. The count only
/// has to separate a populated copy from an empty one, so it stops early.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const WATCHER_DIRECTORY_SIZE_SAMPLE_CAP: usize = 1024;
/// Entries read from an observed prefix when looking for the ignored child that
/// produced its events.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const WATCHER_OBSERVED_CHILD_SCAN_CAP: usize = 256;
/// Candidates named in the overflow log after the cap is reached, so a specimen
/// says what the cap cost.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const WATCHER_DROPPED_CANDIDATE_LOG_LIMIT: usize = 3;
const ROOT_DELETED_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const GITIGNORE_REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DISPATCH_SEND_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How often the watcher thread retries handing held events to a dispatch
/// channel that was full. It sleeps on its raw-event channel in between, so a
/// consumer that has stopped draining (an unbound root) costs no CPU here.
const DISPATCH_BACKLOG_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// Most changed paths the watcher thread holds while the dispatch channel is
/// full. Beyond this they are replaced by one rescan request, the same
/// recovery the operating system's own event overflow takes.
const WATCHER_DISPATCH_BACKLOG_MAX_PATHS: usize = 16 * WATCHER_MAX_BATCH_PATHS;
const WATCHER_ATTRIBUTION_RING_CAPACITY: usize = 512;
const WATCHER_OVERFLOW_PREFIX_LIMIT: usize = 5;
const WATCHER_OBSERVED_EXCLUSION_LIMIT: usize = 32;
const WATCHER_OVERFLOW_QUIET_INTERVAL: Duration = Duration::from_secs(1);
// Ten seconds keeps a sustained flood visible without logging every overflow.
const WATCHER_OVERFLOW_MAX_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) fn rewrite_nested_ignore_line(relative_dir: &Path, line: &str) -> Option<String> {
    if line.trim().is_empty() || line.starts_with('#') {
        return None;
    }

    let escaped_dir = relative_dir
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .chars()
                .flat_map(|character| {
                    if matches!(character, '[' | '*' | '?' | '\\') {
                        [Some('\\'), Some(character)]
                    } else {
                        [Some(character), None]
                    }
                })
                .flatten()
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    let (negation, pattern) = line
        .strip_prefix('!')
        .map_or(("", line), |pattern| ("!", pattern));
    let has_non_trailing_slash = pattern.strip_suffix('/').unwrap_or(pattern).contains('/');
    let rewritten = if let Some(rest) = pattern.strip_prefix("**/") {
        format!("{escaped_dir}/**/{rest}")
    } else if has_non_trailing_slash {
        format!(
            "{escaped_dir}/{}",
            pattern.strip_prefix('/').unwrap_or(pattern)
        )
    } else {
        format!("{escaped_dir}/**/{pattern}")
    };
    Some(format!("{negation}{rewritten}"))
}

#[derive(Debug, Clone)]
pub struct WatcherFilterConfig {
    pub project_root: PathBuf,
    pub git_common_dir: Option<PathBuf>,
    counters: Arc<crate::context::WatcherCounters>,
    /// Raised when `<root>/.cortexkit/aft.jsonc` changes, so the root re-reads
    /// its config. Checked before any ignore filtering: the file is config
    /// whether or not the project ignores `.cortexkit`.
    config_reload: Option<Arc<crate::config_live::ConfigReloadSignal>>,
}

impl WatcherFilterConfig {
    pub fn new(project_root: PathBuf, git_common_dir: Option<PathBuf>) -> Self {
        let counters = crate::context::watcher_counters_for_root(&project_root);
        Self {
            project_root,
            git_common_dir,
            counters,
            config_reload: None,
        }
    }

    pub fn with_config_reload_signal(
        mut self,
        signal: Arc<crate::config_live::ConfigReloadSignal>,
    ) -> Self {
        self.config_reload = Some(signal);
        self
    }

    fn note_config_file_events<'a>(&self, raw_paths: impl IntoIterator<Item = &'a PathBuf>) {
        let Some(signal) = self.config_reload.as_ref() else {
            return;
        };
        if raw_paths
            .into_iter()
            .any(|path| crate::config_live::is_project_config_event_path(&self.project_root, path))
        {
            signal.request();
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
    /// Every path had only explicit data modifications, never topology events.
    ContentOnlyPaths(Vec<PathBuf>),
    /// Ordered control; preceding changes must enter the driver before its ack.
    CookieObserved(PathBuf),
    RescanRequired(RescanReason),
    IgnoreRulesChanged {
        paths: Vec<PathBuf>,
    },
    RootDeleted,
    Error(String),
}

pub(crate) fn watcher_event_is_content_only(kind: &notify::EventKind) -> bool {
    // Modify(Any), renames and backend-specific metadata events do not prove
    // that the path already existed. Only explicit data modifications qualify.
    matches!(
        kind,
        notify::EventKind::Modify(notify::event::ModifyKind::Data(_))
    )
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
    // Operating-system metadata files (a Finder `.DS_Store` rewrite, say) are
    // never indexed by the project walks, so their events carry no change.
    if crate::watcher_backend::cookie::is_cookie_path(path)
        || crate::project_content::is_excluded_event(path)
    {
        return true;
    }
    watcher_path_has_infra_component(path)
}

fn watcher_path_has_infra_component(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, Component::Normal(name) if matches!(
            name.to_str().unwrap_or(""),
            ".git" | ".opencode" | ".alfonso" | ".gsd" | "node_modules" | "target"
        ))
    })
}

pub(crate) fn watcher_path_is_infra_skip_under(root: &Path, path: &Path) -> bool {
    let absolute = root.join(path);
    // Only component matching is root-relative. Filesystem type checks must
    // use the project path, never a relative path interpreted against daemon cwd.
    crate::project_content::is_excluded_event(&absolute)
        || absolute
            .strip_prefix(root)
            .is_ok_and(watcher_path_has_infra_component)
        // Events under a nested linked worktree of this repository belong to
        // that checkout's own session, like other excluded paths. The boundary
        // resolves the common dir lazily and only reads a `.git` file for
        // directories that hold one.
        || crate::project_content::NestedWorktreeBoundary::for_root(root)
            .is_under_nested_worktree(&absolute)
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

struct WatcherBatchMetadata {
    head_paths: BTreeSet<PathBuf>,
    external_ignore_paths: BTreeSet<PathBuf>,
}

impl WatcherBatchMetadata {
    fn resolve(config: &WatcherFilterConfig) -> Self {
        fn add_spellings(paths: &mut BTreeSet<PathBuf>, path: &Path) {
            paths.insert(path.to_path_buf());
            if let Ok(canonical) = std::fs::canonicalize(path) {
                paths.insert(canonical);
            }
        }
        let mut external_ignore_paths = BTreeSet::new();
        add_spellings(&mut external_ignore_paths, &config.git_info_exclude_path());
        #[cfg(test)]
        crate::search_index::audit_record(|work| work.watcher_global += 1);
        if let Some(global) = ignore::gitignore::gitconfig_excludes_path() {
            add_spellings(&mut external_ignore_paths, &global);
        }
        let mut head_paths = BTreeSet::new();
        #[cfg(test)]
        crate::search_index::audit_record(|work| work.watcher_head += 1);
        if let Ok(metadata) = crate::alias::capture_git_head_metadata(
            &config.project_root,
            config.git_common_dir.as_deref(),
        ) {
            for path in metadata.watch_paths() {
                add_spellings(&mut head_paths, path);
            }
        }
        Self {
            head_paths,
            external_ignore_paths,
        }
    }

    fn can_change_corpus_ignore(&self, config: &WatcherFilterConfig, path: &Path) -> bool {
        self.external_ignore_paths.contains(path)
            || (path.starts_with(&config.project_root)
                && watcher_path_is_ignore_file(path)
                && !watcher_path_is_infra_skip_under(&config.project_root, path))
    }
}

pub fn canonicalize_watcher_path(path: PathBuf) -> PathBuf {
    #[cfg(all(target_os = "linux", test))]
    crate::watcher_backend::block_path_resolution_for_test(&path);
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

pub(crate) fn watcher_path_is_ignored_by_matcher(
    matcher: &SharedGitignore,
    path: &Path,
    root: &Path,
) -> bool {
    watcher_path_is_ignored_by_matcher_with_kind(matcher, path, root, false)
}

#[cfg(target_os = "linux")]
pub(crate) fn watcher_directory_is_ignored_by_matcher(
    matcher: &SharedGitignore,
    path: &Path,
    root: &Path,
) -> bool {
    watcher_path_is_ignored_by_matcher_with_kind(matcher, path, root, true)
}

pub(crate) fn watcher_path_is_ignored_by_matcher_with_kind(
    matcher: &SharedGitignore,
    path: &Path,
    root: &Path,
    is_dir: bool,
) -> bool {
    if watcher_path_is_infra_skip_under(root, path) {
        return true;
    }

    with_matcher_snapshot(matcher, |snapshot| {
        watcher_path_is_ignored(snapshot, path, is_dir)
    })
}

pub(crate) fn queued_path_is_ignored_by_matcher(matcher: &SharedGitignore, path: &Path) -> bool {
    // The raw filter deliberately admits HEAD/ref metadata as control events.
    // Retirement must not turn those already-admitted events into infra skips.
    if path.components().any(|part| part.as_os_str() == ".git") {
        return false;
    }
    with_matcher_snapshot(matcher, |snapshot| {
        watcher_path_is_ignored(snapshot, path, false)
    })
}

fn with_matcher_snapshot<T>(
    matcher: &SharedGitignore,
    apply: impl FnOnce(Option<&Gitignore>) -> T,
) -> T {
    // Filesystem work uses an immutable snapshot, not the rebuild writer's lock.
    let snapshot = matcher
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    apply(snapshot.as_deref())
}

fn watcher_path_is_ignored(matcher: Option<&Gitignore>, path: &Path, is_dir: bool) -> bool {
    matcher.is_some_and(|matcher| {
        let joined;
        let path = if path.is_relative() {
            joined = matcher.path().join(path);
            joined.as_path()
        } else {
            path
        };
        // Event paths and the matcher share the watch root's spelling. Keep
        // matching lexical: resolving a replacement symlink (including via
        // is_dir) could wait indefinitely for an unavailable drive.
        path.starts_with(matcher.path())
            && matcher
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore()
    })
}

/// [`watcher_path_is_ignored`] for a path known to be a directory.
///
/// Directory-only rules (`tmp/`) match only when the matcher is told the path
/// is a directory, and a directory that was deleted after its events arrived
/// no longer answers `is_dir()`. Overflow attribution walks the ancestors of
/// event paths, which are directories by construction, so it states that
/// instead of asking the filesystem.
fn watcher_directory_is_ignored(matcher: Option<&Gitignore>, path: &Path) -> bool {
    watcher_path_is_ignored(matcher, path, true)
}

/// True when `path` is an in-tree ignore rule file inside an ignored directory.
/// A rule file that matches itself still supplies rules for its directory and
/// must be reloaded when its contents change.
fn ignore_file_is_ignored(matcher: Option<&Gitignore>, path: &Path) -> bool {
    if !watcher_path_is_ignore_file(path) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    matcher.is_some_and(|matcher| {
        path.starts_with(matcher.path())
            && parent != matcher.path()
            && matcher
                .matched_path_or_any_parents(parent, true)
                .is_ignore()
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
const WATCHER_ECOSYSTEM_EXCLUSIONS: [&str; 17] = [
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "build",
    "dist",
    ".next",
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
    External,
    Ecosystem,
    Observed,
    Gitignore,
}

/// How far one installed exclusion reaches.
///
/// This is a statement about the kernel interface, not about the directory: it
/// decides whether excluding `node_modules` at the root says anything at all
/// about `packages/plugin/node_modules`. Which one the running backend has is
/// named once, in `watcher_backend`, so no rule here has to spell out a
/// platform.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherExclusionCoverage {
    /// The exclusion covers the whole subtree below the excluded directory:
    /// watches are added one directory at a time and an excluded directory is
    /// never descended into.
    Subtree,
    /// The kernel matches the exact path it was handed, so an excluded parent
    /// covers nothing below it and every copy that floods needs its own entry.
    ExactPath,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl WatcherExclusionSource {
    /// Slot ranking between sources, best first.
    fn priority(self) -> u8 {
        match self {
            Self::External => 0,
            Self::Ecosystem => 1,
            Self::Observed => 2,
            Self::Gitignore => 3,
        }
    }

    /// How directly this source knows the directory is heavy, best first. Used
    /// only when two producers describe the same path: a recorded event count
    /// is a measurement, an ecosystem name is a well-founded guess, and an
    /// ignore rule only says the directory is not tracked.
    fn evidence_rank(self) -> u8 {
        match self {
            Self::External => 0,
            Self::Observed => 1,
            Self::Ecosystem => 2,
            Self::Gitignore => 3,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Ecosystem => "ecosystem",
            Self::Observed => "observed",
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

/// Installed exclusions retain their source so matcher changes invalidate only
/// Gitignore entries. External paths describe filesystem topology instead: a
/// link can become source code without any ignore rule changing, so events must
/// recheck those paths with lstat.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) struct WatcherExclusionPaths {
    lasting: Vec<WatcherExclusion>,
    external: BTreeSet<PathBuf>,
    boundary: Option<WatcherWalkBoundary>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl WatcherExclusionPaths {
    pub(crate) fn new(root: &Path, exclusions: &[WatcherExclusion]) -> Self {
        let mut state = Self {
            lasting: Vec::new(),
            external: BTreeSet::new(),
            // Matcher publications do not change the bound root or its device.
            boundary: WatcherWalkBoundary::for_root(root),
        };
        state.replace(exclusions);
        state
    }

    pub(crate) fn replace(&mut self, exclusions: &[WatcherExclusion]) {
        self.lasting = exclusions
            .iter()
            .filter(|entry| entry.source() != WatcherExclusionSource::External)
            .cloned()
            .collect();
        self.external = exclusions
            .iter()
            .filter(|entry| entry.source() == WatcherExclusionSource::External)
            .map(|entry| entry.path.clone())
            .collect();
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn boundary(&self) -> Option<&WatcherWalkBoundary> {
        self.boundary.as_ref()
    }

    // Only the Linux and macOS watcher backends, plus tests on all platforms,
    // read the combined exclusion list.
    #[cfg(any(unix, test))]
    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        self.lasting
            .iter()
            .map(|entry| entry.path.clone())
            .chain(self.external.iter().cloned())
            .collect()
    }

    fn retain_ignored(&mut self, matcher: &SharedGitignore) -> bool {
        let guard = matcher
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only Gitignore entries depend on the current ignore matcher. Keep
        // exclusions with other sources, including external boundaries, until
        // a completed plan or topology event changes them. Avoid filesystem I/O
        // so a stalled drive cannot delay restoring newly unignored watches.
        let previous_len = self.lasting.len();
        self.lasting.retain(|entry| {
            entry.source() != WatcherExclusionSource::Gitignore
                || guard.as_deref().is_some_and(|matcher| {
                    entry.path.starts_with(matcher.path())
                        && matcher
                            .matched_path_or_any_parents(&entry.path, true)
                            .is_ignore()
                })
        });
        self.lasting.len() != previous_len
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn refresh_event(&mut self, event: &notify::Event) -> Vec<PathBuf> {
        self.refresh_events(std::iter::once(event))
    }

    pub(crate) fn refresh_events<'a>(
        &mut self,
        events: impl IntoIterator<Item = &'a notify::Event>,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut rescan = false;
        // Content, metadata and access events cannot replace a directory.
        // Keep their high-volume path free of filesystem probes.
        for event in events {
            if event.need_rescan() {
                rescan = true;
            } else if !matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) {
                continue;
            }
            paths.extend(event.paths.iter().cloned());
        }
        if paths.is_empty() && !rescan {
            return Vec::new();
        }
        let previous = self.external.clone();
        if rescan {
            // Overflow events may have no paths. Recheck every known external
            // boundary, including replacements whose topology event was lost.
            paths.extend(self.external.iter().cloned());
        }
        self.refresh_touched(&paths);
        self.external
            .symmetric_difference(&previous)
            .cloned()
            .collect()
    }

    /// Use lexical event paths, before downstream canonicalization. Checking
    /// ancestors also handles a renamed parent of an excluded path, without
    /// probing descendants through a link or a different-device directory.
    pub(crate) fn refresh_touched(&mut self, paths: &[PathBuf]) -> bool {
        let Some(boundary) = self.boundary.as_ref() else {
            return false;
        };
        let previous = self.external.clone();
        // Component results are keyed by lexical path and shared only within
        // this batch. Discarding the cache at the next batch invalidates both a
        // touched path and descendants of a renamed/replaced parent, including
        // external ancestors reported only through a child event. Walks retain
        // their independent uncached boundary checks.
        let mut cache = BTreeMap::new();
        self.external.retain(|excluded| {
            !paths
                .iter()
                .any(|path| path.starts_with(excluded) || excluded.starts_with(path))
                || boundary.classify_cached(excluded, &mut cache).0 == ExclusionPathKind::External
        });
        let mut seen = BTreeSet::new();
        for path in paths {
            // Discovery need not spend more exclusion slots below an installed
            // lasting exclusion. Probe through that boundary in case it became
            // a link, not through every build artifact. This only shortens the
            // probe: exact-path native exclusions may still deliver descendants,
            // and their events must still reach the ordinary corpus filter.
            let probe = self
                .lasting
                .iter()
                .map(|entry| entry.path())
                .filter(|excluded| path.starts_with(excluded))
                .min_by_key(|excluded| excluded.components().count())
                .unwrap_or(path);
            if !seen.insert(probe) {
                continue;
            }
            if let (_, Some(external)) = boundary.classify_cached(probe, &mut cache) {
                // Keep the shared budget within FSEvents' eight-exclusion limit.
                // Full slots do not permit traversal: every directory scan
                // independently checks symlinks and device boundaries.
                if self.lasting.len() + self.external.len() < WATCHER_EXCLUSION_LIMIT {
                    self.external.insert(external);
                }
            }
        }
        self.external != previous
    }
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
fn root_has_python_manifest(root: &Path) -> bool {
    if exclusion_manifest_is_file(&root.join("pyproject.toml"))
        || exclusion_manifest_is_file(&root.join("requirements.txt"))
    {
        return true;
    }
    fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            entry.file_type().is_ok_and(|file_type| file_type.is_file())
                && name.starts_with("requirements-")
                && name.ends_with(".txt")
        })
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn root_has_gradle_manifest(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_file = entry.file_type().is_ok_and(|file_type| file_type.is_file());
            is_file
                && (matches!(name.as_ref(), "build.gradle" | "build.gradle.kts")
                    || name.starts_with("settings.gradle"))
        })
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn root_has_xcode_project(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".xcodeproj") || name.ends_with(".xcworkspace")
        })
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn ecosystem_exclusion_priority(root: &Path, name: &str) -> Option<usize> {
    let priority = WATCHER_ECOSYSTEM_EXCLUSIONS
        .iter()
        .position(|candidate| *candidate == name)?;
    let enabled = match name {
        ".git" => fs::symlink_metadata(root.join(".git")).is_ok_and(|metadata| metadata.is_dir()),
        "target" => exclusion_manifest_is_file(&root.join("Cargo.toml")),
        "node_modules" => exclusion_manifest_is_file(&root.join("package.json")),
        ".venv" | "venv" | "__pycache__" => root_has_python_manifest(root),
        ".gradle" => root_has_gradle_manifest(root),
        ".dart_tool" => exclusion_manifest_is_file(&root.join("pubspec.yaml")),
        "Pods" => exclusion_manifest_is_file(&root.join("Podfile")),
        "DerivedData" => root_has_xcode_project(root),
        // Generic build outputs: too many ecosystems write these to gate on
        // one manifest (`build/` is Gradle, setuptools, and half of the JS
        // bundlers at once).
        "build" | "dist" | ".next" | ".turbo" | ".cache" | "coverage" | "out" => true,
        _ => false,
    };
    enabled.then_some(priority)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn exclusion_manifest_is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Classify every component before probing descendants: lstat of a deep path
/// alone would still follow a symlink in one of its parent components.
#[derive(Clone)]
pub(crate) struct WatcherWalkBoundary {
    root: PathBuf,
    device: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExclusionPathKind {
    LocalDirectory,
    External,
    Absent,
    Other,
}

fn exclusion_device(path: &Path, metadata: &fs::Metadata) -> Option<u64> {
    #[cfg(test)]
    if EXCLUSION_TEST_DEVICE.with(|device| device.borrow().as_deref() == Some(path)) {
        return Some(u64::MAX);
    }
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

impl WatcherWalkBoundary {
    #[cfg(target_os = "linux")]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn for_root(root: &Path) -> Option<Self> {
        let metadata = watcher_boundary_metadata(root).ok()?;
        if !metadata.is_dir() {
            return None;
        }
        Some(Self {
            root: root.to_path_buf(),
            device: exclusion_device(root, &metadata),
        })
    }

    fn kind(&self, path: &Path) -> ExclusionPathKind {
        self.classify(path).0
    }

    fn classify(&self, path: &Path) -> (ExclusionPathKind, Option<PathBuf>) {
        self.classify_with(path, |path| self.component_kind(path))
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    fn classify_cached(
        &self,
        path: &Path,
        cache: &mut BTreeMap<PathBuf, ExclusionPathKind>,
    ) -> (ExclusionPathKind, Option<PathBuf>) {
        self.classify_with(path, |path| {
            *cache
                .entry(path.to_path_buf())
                .or_insert_with(|| self.component_kind(path))
        })
    }

    fn component_kind(&self, path: &Path) -> ExclusionPathKind {
        let metadata = match watcher_boundary_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ExclusionPathKind::Absent;
            }
            Err(_) => return ExclusionPathKind::Other,
        };
        if metadata.file_type().is_symlink() {
            return ExclusionPathKind::External;
        }
        if !metadata.is_dir() {
            return ExclusionPathKind::Other;
        }
        if self
            .device
            .zip(exclusion_device(path, &metadata))
            .is_some_and(|(root, child)| !crate::walk_boundary::same_filesystem_device(root, child))
        {
            return ExclusionPathKind::External;
        }
        ExclusionPathKind::LocalDirectory
    }

    fn classify_with(
        &self,
        path: &Path,
        mut component_kind: impl FnMut(&Path) -> ExclusionPathKind,
    ) -> (ExclusionPathKind, Option<PathBuf>) {
        #[cfg(test)]
        EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| calls.set(calls.get() + 1));
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return (ExclusionPathKind::Other, None);
        };
        let mut current = self.root.clone();
        for component in relative.components() {
            if !matches!(component, Component::Normal(_)) {
                return (ExclusionPathKind::Other, None);
            }
            current.push(component);
            match component_kind(&current) {
                ExclusionPathKind::LocalDirectory => {}
                ExclusionPathKind::External => return (ExclusionPathKind::External, Some(current)),
                kind => return (kind, None),
            }
        }
        (ExclusionPathKind::LocalDirectory, None)
    }

    pub(crate) fn allows_directory(&self, path: &Path) -> bool {
        self.kind(path) == ExclusionPathKind::LocalDirectory
    }
}

fn watcher_boundary_metadata(path: &Path) -> std::io::Result<fs::Metadata> {
    #[cfg(test)]
    {
        EXCLUSION_TEST_LSTATS.with(|calls| calls.set(calls.get() + 1));
        EXCLUSION_TEST_STAT_PATHS.with(|paths| paths.borrow_mut().push(path.to_path_buf()));
    }
    fs::symlink_metadata(path)
}

/// The exclusion set an OS watcher should install, plus what the cap cost.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WatcherExclusionPlan {
    /// Exclusions to install, best first.
    pub(crate) selected: Vec<WatcherExclusion>,
    /// The next candidates that lost a slot to the cap, best first. Named in
    /// the overflow log so a flood specimen shows what the cap left watched.
    pub(crate) dropped: Vec<WatcherExclusion>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExclusionPlanUnavailable {
    CapacityDeadline,
    IoDeadline,
    SpawnFailed,
    WorkerDisconnected,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl ExclusionPlanUnavailable {
    fn as_str(self) -> &'static str {
        match self {
            Self::CapacityDeadline => "capacity_deadline",
            Self::IoDeadline => "io_deadline",
            Self::SpawnFailed => "spawn_failed",
            Self::WorkerDisconnected => "worker_disconnected",
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
type ExclusionPlanOutcome = Result<WatcherExclusionPlan, ExclusionPlanUnavailable>;

#[cfg(any(target_os = "macos", target_os = "linux", test))]
const EXCLUSION_PLAN_MAX_ATTEMPTS: u32 = 8;

/// One retry budget per matcher generation, shared by the native backends.
/// An unavailable plan must not consume a generation: roots near the end of a
/// restart burst may not get a worker before their first deadline expires.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Default)]
pub(crate) struct WatcherExclusionRetry {
    generation: Option<u64>,
    consumed: bool,
    attempts: u32,
    retry_at: Option<Instant>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) enum WatcherExclusionUpdate {
    Planned(WatcherExclusionPlan),
    Reduced,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl WatcherExclusionRetry {
    /// Install completed plans, or remove stale exclusions when a new matcher
    /// cannot be planned yet. Retrying the full plan keeps its own budget.
    pub(crate) fn replacement(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &crate::context::WatcherCounters,
        installed: &mut WatcherExclusionPaths,
        matcher: &SharedGitignore,
        derive: impl FnOnce() -> ExclusionPlanOutcome,
    ) -> Option<WatcherExclusionUpdate> {
        self.replacement_at(
            root,
            generation,
            counters,
            installed,
            matcher,
            Instant::now(),
            derive,
        )
    }

    fn replacement_at(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &crate::context::WatcherCounters,
        installed: &mut WatcherExclusionPaths,
        matcher: &SharedGitignore,
        now: Instant,
        derive: impl FnOnce() -> ExclusionPlanOutcome,
    ) -> Option<WatcherExclusionUpdate> {
        let new_generation = self.generation != Some(generation);
        match self.attempt_at(root, generation, counters, now, derive)? {
            Ok(plan) => {
                installed.replace(&plan.selected);
                Some(WatcherExclusionUpdate::Planned(plan))
            }
            Err(_) if new_generation && installed.retain_ignored(matcher) => {
                Some(WatcherExclusionUpdate::Reduced)
            }
            Err(_) => None,
        }
    }

    pub(crate) fn attempt(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &crate::context::WatcherCounters,
        derive: impl FnOnce() -> ExclusionPlanOutcome,
    ) -> Option<ExclusionPlanOutcome> {
        self.attempt_at(root, generation, counters, Instant::now(), derive)
    }

    fn attempt_at(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &crate::context::WatcherCounters,
        now: Instant,
        derive: impl FnOnce() -> ExclusionPlanOutcome,
    ) -> Option<ExclusionPlanOutcome> {
        if self.generation != Some(generation) {
            *self = Self {
                generation: Some(generation),
                ..Self::default()
            };
        }
        if self.consumed
            || self.attempts >= EXCLUSION_PLAN_MAX_ATTEMPTS
            || self.retry_at.is_some_and(|deadline| now < deadline)
        {
            return None;
        }
        self.attempts += 1;
        let outcome = derive();
        self.record_outcome(root, generation, counters, &outcome);
        Some(outcome)
    }

    /// A plan is not installed on inotify until its directory walk completes.
    /// Retry a failed walk with the same generation's planning budget and health.
    #[cfg(target_os = "linux")]
    pub(crate) fn walk_unavailable(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &crate::context::WatcherCounters,
        reason: ExclusionPlanUnavailable,
    ) {
        self.record_outcome(root, generation, counters, &Err::<(), _>(reason));
    }

    fn record_outcome<T>(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &crate::context::WatcherCounters,
        outcome: &Result<T, ExclusionPlanUnavailable>,
    ) {
        let (status, last_failure) = match outcome {
            Ok(_) => {
                self.consumed = true;
                self.retry_at = None;
                ("planned", None)
            }
            Err(reason) => {
                self.consumed = false;
                // Back off after completion, not from the start of an attempt
                // that may have spent its entire deadline in filesystem I/O.
                let backoff = Duration::from_millis(250 * (1_u64 << (self.attempts - 1).min(4)));
                self.retry_at = Some(Instant::now() + backoff);
                if self.attempts == 1 {
                    emit_exclusion_retry_log(format!(
                        "watcher exclusion plan unavailable: root={} generation={generation} reason={}; rechecking ignore-derived exclusions during retry (limit={EXCLUSION_PLAN_MAX_ATTEMPTS})",
                        root.display(), reason.as_str()
                    ));
                }
                (
                    if self.attempts == EXCLUSION_PLAN_MAX_ATTEMPTS {
                        "exhausted"
                    } else {
                        "retrying"
                    },
                    Some(reason.as_str()),
                )
            }
        };
        counters.set_exclusion_planning(crate::context::WatcherExclusionPlanningSnapshot {
            matcher_generation: generation,
            attempts: self.attempts,
            attempt_limit: EXCLUSION_PLAN_MAX_ATTEMPTS,
            status,
            last_failure,
        });
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn emit_exclusion_retry_log(line: String) {
    log::warn!("{line}");
    #[cfg(test)]
    EXCLUSION_RETRY_LOGS.with(|logs| logs.borrow_mut().push(line));
}

#[cfg(test)]
thread_local! {
    static EXCLUSION_RETRY_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Cheap size signal for one directory: how many entries its top level holds,
/// counted up to `WATCHER_DIRECTORY_SIZE_SAMPLE_CAP`.
///
/// Nothing recursive happens here. The number only has to separate a populated
/// copy of a name from an empty one when both compete for the same slot, and a
/// capped `read_dir` is the one way to ask that on every platform (`st_blocks`
/// is Unix-only).
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn directory_size_signal(boundary: &WatcherWalkBoundary, path: &Path) -> usize {
    if !boundary.allows_directory(path) {
        return 0;
    }
    match exclusion_read_dir(path) {
        Ok(entries) => entries.take(WATCHER_DIRECTORY_SIZE_SAMPLE_CAP).count(),
        Err(_) => 0,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn exclusion_read_dir(path: &Path) -> std::io::Result<fs::ReadDir> {
    #[cfg(test)]
    EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().push(path.to_path_buf()));
    fs::read_dir(path)
}

#[cfg(test)]
thread_local! {
    pub(crate) static EXCLUSION_TEST_OPENS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(crate) static EXCLUSION_TEST_LSTATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EXCLUSION_TEST_STAT_PATHS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    static WATCHER_TEST_ROOT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static EXCLUSION_TEST_CLASSIFICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static EXCLUSION_TEST_DEVICE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Git metadata is an exclusion boundary, never a tree to enumerate.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn exclusion_walk_skips_git_directory(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new(".git"))
}

/// Choose ignored directory boundaries that an OS watcher can omit entirely.
///
/// Slots are scarce (`WATCHER_EXCLUSION_LIMIT` of them), so the ranking decides
/// what the kernel stops reporting:
///
/// 1. A checkout's `.git` directory owns slot zero; linked worktrees use a
///    `.git` file and do not seed that path.
/// 2. Symlinks and directories on another device are excluded without opening
///    them; indexing does not traverse them either. They precede optional
///    size/event-based choices within the backend's exclusion budget.
/// 3. Once an overflow has recorded event volume, directories with observed
///    volume rank next, heaviest first, ahead of every unobserved candidate.
///    Observed volume is the only direct evidence of which directory fills
///    the kernel queue; an unobserved `node_modules` copy is only a guess.
///    Ranking is not admission, though: one existing representative of each
///    enabled ecosystem keeps a reserved slot, because the next build or
///    install writes into it. When the budget cannot hold every observed
///    directory and every such representative, observed directories get the
///    larger half of the slots after `.git` and representatives the rest.
///    Volume is recorded against the highest ignored ancestor of each event
///    (see `observed_exclusion_prefixes`), so a flood spread over sibling
///    folders of one ignored directory costs a single slot; siblings under a
///    directory that is not itself ignored cannot collapse, since excluding
///    that parent would hide files it tracks.
/// 4. When no candidate has observed event volume, one representative of
///    every enabled ecosystem name ranks before a second copy of any name.
///    This fallback breadth keeps one workspace ecosystem from spending the
///    whole kernel budget on sibling directories. An absent representative is
///    deliberate: watcher backends accept absent exclusions, which begin
///    covering the path if a build creates it later.
/// 5. Representatives follow ecosystem priority. On an exact-path backend the
///    representative is the largest existing copy; on a subtree backend it is
///    the shallowest copy because that path covers its descendants.
/// 6. Remaining candidates preserve the ordinary storm ranking: directories
///    that exist before names that do not, then ecosystem names, observed
///    directories, and ignored boundaries in their source-specific order.
///
/// Ecosystem names are bare (`node_modules`), and a bare gitignore pattern
/// matches at every depth, so every existing copy of each ecosystem directory
/// is seeded with its own path through `WATCHER_NESTED_ECOSYSTEM_MAX_DEPTH`
/// when slots remain. Additional exact-path copies are ordered by a cheap size
/// signal.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn derive_watcher_exclusion_plan(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
) -> ExclusionPlanOutcome {
    #[cfg(test)]
    if forced_exclusion_failures()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(root)
    {
        return Err(ExclusionPlanUnavailable::IoDeadline);
    }
    let root = root.to_path_buf();
    let matcher = matcher.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let result = run_bounded_exclusion_planner(
        Duration::from_secs(2),
        exclusion_planner_pool(),
        move || {
            derive_exclusion_plan_cancellable(
                &root,
                &matcher,
                max_paths,
                crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
                Some(&worker_cancelled),
            )
        },
    );
    // A timed-out plan has no consumer. Stop its walk at the next filesystem
    // checkpoint instead of leaving a detached helper traversing the root.
    cancelled.store(true, Ordering::Release);
    result
}

#[cfg(test)]
fn forced_exclusion_failures() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) struct ForcedExclusionFailure(PathBuf);

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl Drop for ForcedExclusionFailure {
    fn drop(&mut self) {
        forced_exclusion_failures()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) fn force_exclusion_failure_for_test(root: &Path) -> ForcedExclusionFailure {
    let root = root.canonicalize().unwrap();
    forced_exclusion_failures()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root.clone());
    ForcedExclusionFailure(root)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn exclusion_planner_pool() -> Arc<ExclusionPlannerPool> {
    static PLANNERS: std::sync::OnceLock<Arc<ExclusionPlannerPool>> = std::sync::OnceLock::new();
    PLANNERS
        .get_or_init(|| Arc::new(ExclusionPlannerPool::default()))
        .clone()
}

/// Wait for bounded filesystem work away from the event-forwarding thread.
/// The coordinator never does filesystem I/O; the inner helper shares the
/// planner's two permits, including permits held by timed-out, stuck workers.
#[cfg(target_os = "linux")]
pub(crate) fn start_watcher_rebuild<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<mpsc::Receiver<Result<T, ExclusionPlanUnavailable>>, ExclusionPlanUnavailable> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("aft-watch-rebuild".into())
        .spawn(move || {
            let result = run_bounded_exclusion_planner(
                Duration::from_secs(2),
                exclusion_planner_pool(),
                work,
            );
            let _ = tx.send(result);
        })
        .map_err(|_| ExclusionPlanUnavailable::SpawnFailed)?;
    Ok(rx)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Default)]
struct ExclusionPlannerPool {
    in_flight: Mutex<usize>,
    available: std::sync::Condvar,
}

/// Directory opens and even lstat can stall in the kernel. A deadline in the
/// walking loop cannot interrupt them, so isolate the entire optional plan.
/// Timed-out workers retain their permit until they exit; at most two can be
/// stuck across all roots. Wait for capacity within the same caller deadline:
/// a restart can re-derive many roots at once, and backends consume each matcher
/// generation only after a successful plan. Deadline and worker failures are
/// unavailable outcomes, not successful empty plans; the backend can retry them.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn run_bounded_exclusion_planner<T: Send + 'static>(
    timeout: Duration,
    pool: Arc<ExclusionPlannerPool>,
    plan: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ExclusionPlanUnavailable> {
    let deadline = Instant::now() + timeout;
    let mut in_flight = pool.in_flight.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ExclusionPlanUnavailable::CapacityDeadline);
        }
        if *in_flight < 2 {
            break;
        }
        (in_flight, _) = pool
            .available
            .wait_timeout(in_flight, remaining)
            .unwrap_or_else(|e| e.into_inner());
    }
    *in_flight += 1;
    drop(in_flight);
    struct Permit(Arc<ExclusionPlannerPool>);
    impl Drop for Permit {
        fn drop(&mut self) {
            let mut in_flight = self.0.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            *in_flight -= 1;
            self.0.available.notify_one();
        }
    }
    let permit = Permit(pool);
    let (tx, rx) = mpsc::sync_channel(1);
    if thread::Builder::new()
        .name("aft-exclusion-plan".into())
        .spawn(move || {
            let _permit = permit;
            let _ = tx.send(plan());
        })
        .is_err()
    {
        return Err(ExclusionPlanUnavailable::SpawnFailed);
    }
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => ExclusionPlanUnavailable::IoDeadline,
            mpsc::RecvTimeoutError::Disconnected => ExclusionPlanUnavailable::WorkerDisconnected,
        })
}

/// The selected exclusions of [`derive_watcher_exclusion_plan`].
#[cfg(test)]
pub(crate) fn derive_excluded_subtrees(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
) -> Vec<WatcherExclusion> {
    derive_exclusion_plan(
        root,
        matcher,
        max_paths,
        crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
    )
    .selected
}

/// [`derive_watcher_exclusion_plan`] for a stated backend coverage.
#[cfg(test)]
fn derive_exclusion_plan(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
    coverage: WatcherExclusionCoverage,
) -> WatcherExclusionPlan {
    derive_exclusion_plan_cancellable(root, matcher, max_paths, coverage, None)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn derive_exclusion_plan_cancellable(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
    coverage: WatcherExclusionCoverage,
    cancelled: Option<&AtomicBool>,
) -> WatcherExclusionPlan {
    #[derive(Debug)]
    struct Candidate {
        path: PathBuf,
        relative: PathBuf,
        source: WatcherExclusionSource,
        exists: bool,
        ecosystem_priority: usize,
        observed_count: u64,
        size_signal: usize,
        gitignore_order: GitignoreOrder,
    }

    impl Candidate {
        /// Fold in a second reading of the same path.
        ///
        /// Each producer knows a different fact about a directory: the tree
        /// walk knows which ignore rule matched, the overflow ring knows how
        /// many events came out of it, the ecosystem table knows its priority.
        /// Keep every fact and label the candidate by its strongest evidence,
        /// so the order the producers run in cannot change the outcome.
        fn absorb(&mut self, other: Self) {
            if other.source.evidence_rank() < self.source.evidence_rank() {
                self.source = other.source;
            }
            self.exists |= other.exists;
            self.ecosystem_priority = self.ecosystem_priority.min(other.ecosystem_priority);
            self.observed_count = self.observed_count.max(other.observed_count);
            self.size_signal = self.size_signal.max(other.size_signal);
            self.gitignore_order = self.gitignore_order.clone().min(other.gitignore_order);
        }
    }

    fn admit(candidates: &mut BTreeMap<PathBuf, Candidate>, candidate: Candidate) {
        match candidates.get_mut(&candidate.relative) {
            Some(existing) => existing.absorb(candidate),
            None => {
                candidates.insert(candidate.relative.clone(), candidate);
            }
        }
    }

    /// Ecosystem priority for a directory that exists at `depth` below the
    /// root, or `None` when the name is not an enabled ecosystem name or sits
    /// below the enumeration bound.
    fn nested_ecosystem_priority(root: &Path, path: &Path, depth: usize) -> Option<usize> {
        let name = path.file_name()?.to_str()?;
        if name == ".git" {
            return Some(0);
        }
        if depth > WATCHER_NESTED_ECOSYSTEM_MAX_DEPTH {
            return None;
        }
        let mut owner = path.parent()?;
        loop {
            if let Some(priority) = ecosystem_exclusion_priority(owner, name) {
                return Some(priority);
            }
            // Workspace-level manifests govern descendants too, but a nested
            // checkout must not inherit an unrelated session root's ecosystem.
            if owner == root || owner.join(".git").exists() {
                return None;
            }
            owner = owner.parent()?;
        }
    }

    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(boundary) = WatcherWalkBoundary::for_root(&root) else {
        return WatcherExclusionPlan {
            selected: Vec::new(),
            dropped: Vec::new(),
        };
    };
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
    let mut candidates = BTreeMap::<PathBuf, Candidate>::new();
    let mut stack = vec![(root.clone(), 0usize)];

    while let Some((directory, depth)) = stack.pop() {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return WatcherExclusionPlan::default();
        }
        if !boundary.allows_directory(&directory) {
            continue;
        }
        let Ok(entries) = exclusion_read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return WatcherExclusionPlan::default();
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() && !file_type.is_symlink() {
                continue;
            }
            let kind = boundary.kind(&path);
            if kind == ExclusionPathKind::External {
                let relative = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                admit(
                    &mut candidates,
                    Candidate {
                        path,
                        relative,
                        source: WatcherExclusionSource::External,
                        exists: true,
                        ecosystem_priority: usize::MAX,
                        observed_count: 0,
                        size_signal: 0,
                        gitignore_order: GitignoreOrder::default(),
                    },
                );
                continue;
            }
            if kind != ExclusionPathKind::LocalDirectory {
                continue;
            }
            if exclusion_walk_skips_git_directory(&path) {
                let relative = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                let observed_count = observed.get(&relative).copied().unwrap_or_default();
                admit(
                    &mut candidates,
                    Candidate {
                        path,
                        relative,
                        source: if observed_count > 0 {
                            WatcherExclusionSource::Observed
                        } else {
                            WatcherExclusionSource::Ecosystem
                        },
                        exists: true,
                        ecosystem_priority: 0,
                        observed_count,
                        size_signal: 0,
                        gitignore_order: GitignoreOrder::default(),
                    },
                );
                continue;
            }
            let child_depth = depth + 1;
            let matched_glob = matcher.as_deref().and_then(|matcher| {
                match matcher.matched_path_or_any_parents(&path, true) {
                    ignore::Match::Ignore(glob) => Some(glob),
                    ignore::Match::None | ignore::Match::Whitelist(_) => None,
                }
            });
            let Some(glob) = matched_glob else {
                stack.push((path, child_depth));
                continue;
            };
            let relative = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            let observed_count = observed.get(&relative).copied();
            let ecosystem_priority = nested_ecosystem_priority(&root, &path, child_depth);
            let source = if observed_count.is_some() {
                WatcherExclusionSource::Observed
            } else if ecosystem_priority.is_some() {
                WatcherExclusionSource::Ecosystem
            } else {
                WatcherExclusionSource::Gitignore
            };
            let size_signal = if ecosystem_priority.is_some() {
                directory_size_signal(&boundary, &path)
            } else {
                0
            };
            admit(
                &mut candidates,
                Candidate {
                    path,
                    relative,
                    source,
                    exists: true,
                    ecosystem_priority: ecosystem_priority.unwrap_or(usize::MAX),
                    observed_count: observed_count.unwrap_or_default(),
                    size_signal,
                    gitignore_order: GitignoreOrder::for_glob(&root, glob),
                },
            );
        }
    }

    if boundary.allows_directory(&root_git) {
        let relative = PathBuf::from(".git");
        admit(
            &mut candidates,
            Candidate {
                path: root_git,
                relative,
                source: WatcherExclusionSource::Ecosystem,
                exists: true,
                ecosystem_priority: ecosystem_exclusion_priority(&root, ".git")
                    .expect(".git is an ecosystem exclusion"),
                observed_count: 0,
                size_signal: 0,
                gitignore_order: GitignoreOrder::default(),
            },
        );
    }

    if let Some(matcher) = matcher.as_deref() {
        for (relative, observed_count) in &observed {
            let path = root.join(relative);
            let kind = boundary.kind(&path);
            if !matches!(
                kind,
                ExclusionPathKind::LocalDirectory | ExclusionPathKind::Absent
            ) {
                continue;
            }
            match matcher.matched_path_or_any_parents(&path, true) {
                ignore::Match::Ignore(glob) => {
                    let exists = kind == ExclusionPathKind::LocalDirectory;
                    admit(
                        &mut candidates,
                        Candidate {
                            path,
                            relative: relative.clone(),
                            source: WatcherExclusionSource::Observed,
                            exists,
                            ecosystem_priority: usize::MAX,
                            observed_count: *observed_count,
                            size_signal: 0,
                            gitignore_order: GitignoreOrder::for_glob(&root, glob),
                        },
                    );
                }
                ignore::Match::None | ignore::Match::Whitelist(_) => {
                    // The ring records the leading components of the flooding
                    // path, which is often a tracked package directory whose
                    // ignored child does the writing (`packages/plugin` for a
                    // `bun install` into `packages/plugin/node_modules`). A
                    // tracked directory can never be excluded, so credit its
                    // events one level down to the ignored children that can.
                    if kind != ExclusionPathKind::LocalDirectory {
                        continue;
                    }
                    let Ok(entries) = exclusion_read_dir(&path) else {
                        continue;
                    };
                    for entry in entries.flatten().take(WATCHER_OBSERVED_CHILD_SCAN_CAP) {
                        let child = entry.path();
                        if !boundary.allows_directory(&child) {
                            continue;
                        }
                        if exclusion_walk_skips_git_directory(&child) {
                            continue;
                        }
                        let ignore::Match::Ignore(glob) =
                            matcher.matched_path_or_any_parents(&child, true)
                        else {
                            continue;
                        };
                        let child_relative =
                            child.strip_prefix(&root).unwrap_or(&child).to_path_buf();
                        let child_depth = child_relative.components().count();
                        let ecosystem_priority =
                            nested_ecosystem_priority(&root, &child, child_depth);
                        let size_signal = if ecosystem_priority.is_some() {
                            directory_size_signal(&boundary, &child)
                        } else {
                            0
                        };
                        admit(
                            &mut candidates,
                            Candidate {
                                path: child,
                                relative: child_relative,
                                source: WatcherExclusionSource::Observed,
                                exists: true,
                                ecosystem_priority: ecosystem_priority.unwrap_or(usize::MAX),
                                observed_count: *observed_count,
                                size_signal,
                                gitignore_order: GitignoreOrder::for_glob(&root, glob),
                            },
                        );
                    }
                }
            }
        }

        for name in WATCHER_ECOSYSTEM_EXCLUSIONS {
            if name == ".git" {
                continue;
            }
            let Some(ecosystem_priority) = ecosystem_exclusion_priority(&root, name) else {
                continue;
            };
            let relative = PathBuf::from(name);
            let path = root.join(&relative);
            let kind = boundary.kind(&path);
            if !matches!(
                kind,
                ExclusionPathKind::LocalDirectory | ExclusionPathKind::Absent
            ) {
                continue;
            }
            let ignore::Match::Ignore(glob) = matcher.matched_path_or_any_parents(&path, true)
            else {
                continue;
            };
            let exists = kind == ExclusionPathKind::LocalDirectory;
            let size_signal = if exists {
                directory_size_signal(&boundary, &path)
            } else {
                0
            };
            admit(
                &mut candidates,
                Candidate {
                    path,
                    relative,
                    source: WatcherExclusionSource::Ecosystem,
                    exists,
                    ecosystem_priority,
                    observed_count: 0,
                    size_signal,
                    gitignore_order: GitignoreOrder::for_glob(&root, glob),
                },
            );
        }
    }

    let mut candidates = candidates.into_values().collect::<Vec<_>>();

    // When no event-volume evidence exists, reserve breadth before depth. The
    // representative set is computed before the final sort so a repeated name
    // cannot push another ecosystem behind the slot cap merely because its
    // directories already exist.
    let has_observed_candidates = candidates
        .iter()
        .any(|candidate| candidate.source == WatcherExclusionSource::Observed);
    let mut ecosystem_representatives = BTreeMap::<usize, usize>::new();
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        if candidate.ecosystem_priority == usize::MAX {
            continue;
        }
        ecosystem_representatives
            .entry(candidate.ecosystem_priority)
            .and_modify(|representative_index| {
                let representative = &candidates[*representative_index];
                let candidate_depth = candidate.relative.components().count();
                let representative_depth = representative.relative.components().count();
                let candidate_is_better = match coverage {
                    // Shallower covers more only when the shallow path is an
                    // ancestor of the deeper one. A root `dist` is not an
                    // ancestor of `packages/plugin/dist`, so preferring depth
                    // first would hand the slot to an absent sibling that
                    // covers nothing while an existing copy takes the writes.
                    // Existence decides first; depth breaks ties among paths
                    // that are really there, where it does mean coverage.
                    WatcherExclusionCoverage::Subtree => representative
                        .exists
                        .cmp(&candidate.exists)
                        .then_with(|| candidate_depth.cmp(&representative_depth))
                        .then_with(|| representative.size_signal.cmp(&candidate.size_signal))
                        .then_with(|| candidate.path.cmp(&representative.path))
                        .is_lt(),
                    WatcherExclusionCoverage::ExactPath => representative
                        .exists
                        .cmp(&candidate.exists)
                        .then_with(|| representative.size_signal.cmp(&candidate.size_signal))
                        .then_with(|| candidate.path.cmp(&representative.path))
                        .is_lt(),
                };
                if candidate_is_better {
                    *representative_index = candidate_index;
                }
            })
            .or_insert(candidate_index);
    }
    // Representatives that exist on disk hold build output or dependencies
    // right now, so the next build or install writes into them. After an
    // overflow these keep a slot even though they produced no recorded
    // events: evicting them for a busy folder would trade the overflow that
    // just happened for the one the next `cargo build` or `bun install` causes.
    // `.git` is ecosystem priority zero but has its own fixed slot.
    let existing_representatives = ecosystem_representatives
        .values()
        .map(|index| &candidates[*index])
        .filter(|candidate| candidate.exists && candidate.relative != Path::new(".git"))
        .map(|candidate| candidate.relative.clone())
        .collect::<BTreeSet<_>>();
    let ecosystem_representatives = ecosystem_representatives
        .into_values()
        .map(|index| candidates[index].relative.clone())
        .collect::<BTreeSet<_>>();

    // Preserve the bound repository's metadata slot; nested repositories compete
    // for the remaining slots without ever enumerating their metadata trees.
    let is_root_git = |candidate: &Candidate| candidate.relative == Path::new(".git");
    // After an overflow the recorded event volume decides. Ranking a measured
    // directory behind every unobserved ecosystem copy would let a monorepo's
    // existing `node_modules` copies fill all the slots, leaving the directory
    // that actually overflowed the queue watched on the next seed.
    let observed_count = |candidate: &Candidate| {
        if has_observed_candidates && candidate.source == WatcherExclusionSource::Observed {
            candidate.observed_count
        } else {
            0
        }
    };
    candidates.sort_by(|left, right| {
        let left_is_representative =
            !has_observed_candidates && ecosystem_representatives.contains(&left.relative);
        let right_is_representative =
            !has_observed_candidates && ecosystem_representatives.contains(&right.relative);
        is_root_git(right)
            .cmp(&is_root_git(left))
            .then_with(|| {
                (right.source == WatcherExclusionSource::External)
                    .cmp(&(left.source == WatcherExclusionSource::External))
            })
            .then_with(|| observed_count(right).cmp(&observed_count(left)))
            .then_with(|| right_is_representative.cmp(&left_is_representative))
            .then_with(|| {
                if left_is_representative && right_is_representative {
                    left.ecosystem_priority.cmp(&right.ecosystem_priority)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| left.exists.cmp(&right.exists).reverse())
            .then_with(|| left.source.priority().cmp(&right.source.priority()))
            .then_with(|| match left.source {
                WatcherExclusionSource::External => left.path.cmp(&right.path),
                WatcherExclusionSource::Ecosystem => left
                    .ecosystem_priority
                    .cmp(&right.ecosystem_priority)
                    // Two copies of one name: prefer the heavier top level. An
                    // install target holds thousands of entries, a stale copy
                    // holds none.
                    .then_with(|| right.size_signal.cmp(&left.size_signal))
                    .then_with(|| left.path.cmp(&right.path)),
                WatcherExclusionSource::Observed => right
                    .observed_count
                    .cmp(&left.observed_count)
                    .then_with(|| left.path.cmp(&right.path)),
                WatcherExclusionSource::Gitignore => left
                    .gitignore_order
                    .cmp(&right.gitignore_order)
                    .then_with(|| left.path.cmp(&right.path)),
            })
    });

    // Slot budget after an overflow. The sort above puts observed folders
    // first, and admitted in that order a flood spread over many folders
    // would take every slot and push out the existing build-output and
    // dependency folders. Each kind therefore gets a reservation:
    //
    // - existing ecosystem representatives (one per ecosystem name, never a
    //   second copy) and observed folders each keep the slots they need while
    //   the budget holds both;
    // - when it cannot, observed folders get the larger half of the slots
    //   after `.git` (they are the measured cause of the overflow that just
    //   happened) and ecosystem representatives get the rest in ecosystem
    //   priority order, so neither kind can take every slot;
    // - a slot one kind does not need goes to the other, and everything else
    //   (absent seeds, second copies, plain ignored boundaries) fills what is
    //   left in rank order.
    //
    // Without observed volume the ranking alone already places every
    // representative first, so no reservation is applied and a fresh seed is
    // unchanged.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SlotKind {
        RootGit,
        ExistingEcosystem,
        Observed,
        Other,
    }
    let slot_kind = |candidate: &Candidate| {
        if is_root_git(candidate) || candidate.source == WatcherExclusionSource::External {
            SlotKind::RootGit
        } else if has_observed_candidates && existing_representatives.contains(&candidate.relative)
        {
            SlotKind::ExistingEcosystem
        } else if candidate.source == WatcherExclusionSource::Observed {
            SlotKind::Observed
        } else {
            SlotKind::Other
        }
    };
    let limit = max_paths.unwrap_or(usize::MAX);
    let available = limit.saturating_sub(
        candidates
            .iter()
            .filter(|candidate| {
                is_root_git(candidate) || candidate.source == WatcherExclusionSource::External
            })
            .count(),
    );
    let count_kind = |kind: SlotKind| {
        candidates
            .iter()
            .filter(|candidate| slot_kind(candidate) == kind)
            .count()
    };
    let observed_wanted = count_kind(SlotKind::Observed);
    let ecosystem_wanted = count_kind(SlotKind::ExistingEcosystem);
    let mut ecosystem_outstanding =
        ecosystem_wanted.min(available - observed_wanted.min(available.div_ceil(2)));
    let mut observed_outstanding = observed_wanted.min(available - ecosystem_outstanding);

    let mut selected = Vec::<WatcherExclusion>::new();
    let mut dropped = Vec::<WatcherExclusion>::new();
    for candidate in candidates {
        let kind = slot_kind(&candidate);
        if coverage == WatcherExclusionCoverage::Subtree
            && !matches!(
                candidate.source,
                WatcherExclusionSource::Observed | WatcherExclusionSource::External
            )
        {
            // Only an ancestor exclusion covers this subtree. Equal directory
            // names in different repositories never cover one another.
            let nested_copy = selected
                .iter()
                .any(|selected| candidate.path.starts_with(&selected.path));
            if nested_copy {
                // A covered representative needs no slot of its own, so its
                // reservation is released for whatever ranks next.
                if kind == SlotKind::ExistingEcosystem {
                    ecosystem_outstanding = ecosystem_outstanding.saturating_sub(1);
                }
                continue;
            }
        }
        let exclusion = WatcherExclusion {
            path: candidate.path,
            source: candidate.source,
        };
        let reserved_for_others = match kind {
            SlotKind::RootGit => 0,
            SlotKind::ExistingEcosystem => observed_outstanding,
            SlotKind::Observed => ecosystem_outstanding,
            SlotKind::Other => ecosystem_outstanding + observed_outstanding,
        };
        if limit.saturating_sub(selected.len()) > reserved_for_others {
            match kind {
                SlotKind::ExistingEcosystem => {
                    ecosystem_outstanding = ecosystem_outstanding.saturating_sub(1);
                }
                SlotKind::Observed => {
                    observed_outstanding = observed_outstanding.saturating_sub(1);
                }
                SlotKind::RootGit | SlotKind::Other => {}
            }
            selected.push(exclusion);
            continue;
        }
        // Reserved slots can still be claimed further down the ranking, so
        // the scan continues past a full dropped list.
        if dropped.len() < WATCHER_DROPPED_CANDIDATE_LOG_LIMIT {
            dropped.push(exclusion);
        }
    }
    WatcherExclusionPlan { selected, dropped }
}

const WATCHER_OBSERVATION_STATE_PREFIX: &str = "watcher.observed_exclusion_prefixes";

/// Overflow rankings are shared by repository identity so a newly-created
/// linked worktree starts with evidence learned by existing checkouts. We keep
/// one repository ranking rather than a per-root override; the most recently
/// persisted observation becomes the next bind's ranking for every sibling.
fn watcher_observation_state_key(root: &Path) -> String {
    let repository_key = crate::root_cache::configured_artifact_key(root)
        .or_else(|| crate::search_index::artifact_cache_key_memoized_only(root))
        .unwrap_or_else(|| crate::search_index::artifact_cache_key_for_build(root));
    format!("{WATCHER_OBSERVATION_STATE_PREFIX}:{repository_key}")
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
    pub ignore_file_paths: BTreeSet<PathBuf>,
}

fn filter_canonical_paths(
    config: &WatcherFilterConfig,
    matcher: &SharedGitignore,
    raw_paths: BTreeSet<PathBuf>,
) -> FilteredWatcherPaths {
    with_matcher_snapshot(matcher, |matcher| {
        // Resolve control paths once for this flush, not once for every source
        // change. A fresh resolution on the next flush observes HEAD/ref and global
        // ignore configuration changes without a process-lifetime cache.
        let metadata = WatcherBatchMetadata::resolve(config);
        // A `.gitignore` written inside a directory the current rules already
        // ignore cannot change the corpus: git never descends into an ignored
        // directory to read one, and neither does the matcher. wrangler, for one,
        // writes a `.gitignore` into every dev directory it creates under an
        // ignored `.wrangler/tmp/`; treating each as a rule change rebuilt the
        // matcher and cold-rebuilt the search index every few seconds for an hour
        // (2026-09-18). A rule file whose parent is not ignored, or any rule file
        // while no matcher exists yet, still counts.
        let ignore_file_paths = raw_paths
            .iter()
            .filter(|path| {
                metadata.can_change_corpus_ignore(config, path)
                    && !ignore_file_is_ignored(matcher, path)
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let ignore_file_changed = !ignore_file_paths.is_empty();

        let changed = raw_paths
            .into_iter()
            .filter(|path| {
                if metadata.head_paths.contains(path) {
                    return true;
                }
                if watcher_path_is_infra_skip_under(&config.project_root, path) {
                    return false;
                }

                if metadata.external_ignore_paths.contains(path) {
                    return false;
                }

                // raw_paths were canonicalized before the flush. Repeating that
                // I/O here buys nothing and charges every checkout path twice.
                if matcher.is_some_and(|matcher| {
                    path.starts_with(matcher.path())
                        && matcher
                            .matched_path_or_any_parents(path, path.is_dir())
                            .is_ignore()
                }) {
                    return false;
                }
                true
            })
            .collect();

        FilteredWatcherPaths {
            changed,
            ignore_file_changed,
            ignore_file_paths,
        }
    })
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
            let _subscription = crate::watcher_backend::subscription::start(
                &root_path,
                crate::watcher_backend::initial_coverage_of(&_watcher),
            );
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
    content_only_paths: BTreeSet<PathBuf>,
    recent_paths: VecDeque<(PathBuf, Instant)>,
    flush_deadline: Option<Instant>,
    next_root_check: Instant,
    /// Events the dispatch channel had no room for, oldest first. Changed
    /// paths are merged into one event while they wait; see `hold_event`.
    backlog: VecDeque<WatcherDispatchEvent>,
    overflow_burst: Option<(RescanReason, u64, Instant, Instant, bool)>,
    #[cfg(test)]
    delay_before_rescan_for_test: Duration,
}

/// What happened to an event handed to the dispatch channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// It is in the channel.
    Sent,
    /// The channel was full (or older events still wait), so the watcher
    /// thread holds it and retries later.
    Held,
    /// The consumer is gone or shutdown was requested; the thread should stop.
    Closed,
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
            content_only_paths: BTreeSet::new(),
            recent_paths: VecDeque::with_capacity(WATCHER_ATTRIBUTION_RING_CAPACITY),
            flush_deadline: None,
            next_root_check: Instant::now(),
            backlog: VecDeque::new(),
            overflow_burst: None,
            #[cfg(test)]
            delay_before_rescan_for_test: Duration::ZERO,
        }
    }

    fn run(&mut self, raw_rx: mpsc::Receiver<notify::Result<notify::Event>>) {
        self.run_until_stopped(raw_rx);
        self.flush_overflow_summary();
    }

    fn run_until_stopped(&mut self, raw_rx: mpsc::Receiver<notify::Result<notify::Event>>) {
        loop {
            if self.overflow_burst.as_ref().is_some_and(|burst| {
                burst.3.elapsed() >= WATCHER_OVERFLOW_QUIET_INTERVAL
                    || burst.2.elapsed() >= WATCHER_OVERFLOW_MAX_INTERVAL
            }) {
                self.flush_overflow_summary();
            }
            if self.shutdown.load(Ordering::SeqCst) {
                self.flush_pending();
                return;
            }
            if !self.retry_backlog() {
                return;
            }
            if self.project_root_was_deleted(Instant::now()) {
                self.raw_paths.clear();
                self.content_only_paths.clear();
                let _ = self.send_dispatch_final(WatcherDispatchEvent::RootDeleted);
                return;
            }
            if self.flush_deadline_reached() {
                if !self.flush_pending() {
                    return;
                }
                continue;
            }

            match raw_rx.recv_timeout(self.next_recv_timeout()) {
                Ok(Ok(mut event)) => {
                    #[cfg(feature = "test-timing-hooks")]
                    hold_watcher_intake_for_test(&self.shutdown);
                    self.config.counters.note_raw_event();
                    let cookies = self.take_cookie_paths(&mut event);
                    if self.config.counters.bash_changes.get().is_some()
                        && self.config.counters.bash_flush.observe(&event)
                    {
                        continue;
                    }
                    if let Some(journal) = self.config.counters.bash_changes.get() {
                        journal.event_for_root(&event, &self.config.project_root, &self.matcher);
                    }
                    if event.need_rescan() {
                        // Delay only this test's overflow notification so its
                        // timeout is exercised against worker scheduling.
                        #[cfg(test)]
                        if !self.delay_before_rescan_for_test.is_zero() {
                            thread::sleep(self.delay_before_rescan_for_test);
                        }

                        let reason = RescanReason::from_event_info(event.info());
                        // Backend rescan paths do not bound the lost events:
                        // FSEvents can attach an objects path to a stream-wide
                        // drop. The discarded raw buffer can also contain an
                        // ignore-file edit. Neither set of paths proves that
                        // the ignore-rule scan can be skipped.
                        self.config.counters.note_overflow_ignore_relevance(true);
                        let during_rescan = self.log_overflow(reason);
                        self.raw_paths.clear();
                        self.content_only_paths.clear();
                        self.flush_deadline = None;
                        // Dropped events may include a config file edit; the
                        // reload compares contents, so asking is cheap.
                        if let Some(signal) = self.config.config_reload.as_ref() {
                            signal.request();
                        }
                        if !during_rescan
                            && self.send_dispatch(WatcherDispatchEvent::RescanRequired(reason))
                                == Dispatch::Closed
                        {
                            return;
                        }
                        continue;
                    }
                    // Config file edits are recognised on the raw event, before
                    // the corpus filters below: those drop paths under any
                    // `target`/`node_modules`/... component (which may be an
                    // ancestor of the root) and resolve symlinked file names.
                    self.config.note_config_file_events(&event.paths);
                    self.record_recent_paths(&event.paths);
                    if watcher_event_invalidates(&event.kind) {
                        self.config.counters.note_invalidating_event();
                        let content_only = watcher_event_is_content_only(&event.kind);
                        if !self.push_raw_paths_with_kind(event.paths, content_only) {
                            return;
                        }
                    }
                    for cookie in cookies {
                        if !self.flush_pending()
                            || self.send_dispatch(WatcherDispatchEvent::CookieObserved(cookie))
                                == Dispatch::Closed
                        {
                            return;
                        }
                    }
                }
                Ok(Err(error)) => {
                    if let Some(journal) = self.config.counters.bash_changes.get() {
                        journal.gap();
                    }
                    let _ =
                        self.send_dispatch_final(WatcherDispatchEvent::Error(error.to_string()));
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.flush_pending() {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !self.shutdown.load(Ordering::SeqCst) {
                        let _ = self.send_dispatch_final(WatcherDispatchEvent::Error(
                            "watcher channel disconnected".to_string(),
                        ));
                    }
                    return;
                }
            }
        }
    }

    fn take_cookie_paths(&self, event: &mut notify::Event) -> Vec<PathBuf> {
        let acknowledge =
            !event.need_rescan() && matches!(event.kind, notify::EventKind::Create(_));
        let mut cookies = Vec::new();
        event.paths.retain(|path| {
            if !crate::watcher_backend::cookie::is_cookie_path(path) {
                return true;
            }
            // Only root cookies are controls. Do not unlink a similarly named
            // file in an auxiliary watch or a nested directory.
            if path.parent() == Some(self.config.project_root.as_path()) {
                let removed = std::fs::remove_file(path).or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                });
                if acknowledge && removed.is_ok() {
                    cookies.push(path.clone());
                }
            }
            false
        });
        cookies
    }

    fn project_root_was_deleted(&mut self, now: Instant) -> bool {
        // Check on a wall-clock cadence, not once per raw event. Ignored build
        // bursts can otherwise turn the root-deletion fallback into a stat loop.
        if now < self.next_root_check {
            return false;
        }
        self.next_root_check = now + ROOT_DELETED_CHECK_INTERVAL;
        #[cfg(test)]
        WATCHER_TEST_ROOT_PROBES.with(|calls| calls.set(calls.get() + 1));
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
        let mut counts = BTreeMap::<String, u64>::new();
        for (path, _) in &self.recent_paths {
            let mut absolute = self.config.project_root.clone();
            let names = path
                .components()
                .filter_map(|component| match component {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
                .collect::<Vec<_>>();
            for (index, name) in names.iter().enumerate() {
                absolute.push(name);
                // Every component above the event's own path is a directory,
                // whether or not it still exists. The event's own path counts
                // only when it is a directory: an exclusion is a directory, so
                // a count recorded against an ignored file (`.cortexkit/alfonso/
                // notes.md` under a `dir/*` rule) can never match a candidate
                // and only takes a place in the capped observation list.
                let is_leaf = index + 1 == names.len();
                if is_leaf && !absolute.is_dir() {
                    break;
                }
                if absolute.file_name() == Some(std::ffi::OsStr::new(".git"))
                    || watcher_directory_is_ignored(matcher.as_deref(), &absolute)
                {
                    // Recorded with `/` on every platform: the prefix is a
                    // persisted, logged key, and `Path` parses `/` on Windows
                    // too, so candidates still match it there.
                    let key = names[..=index]
                        .iter()
                        .map(|name| name.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    *counts.entry(key).or_default() += 1;
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

    fn log_overflow(&mut self, reason: RescanReason) -> bool {
        let prefixes = self.overflow_prefixes();
        self.config
            .counters
            .set_observed_exclusion_prefixes(self.observed_exclusion_prefixes());
        let during_rescan = self.config.counters.note_overflow(reason, prefixes);
        let now = Instant::now();
        let burst = self
            .overflow_burst
            .get_or_insert((reason, 0, now, now, false));
        burst.1 += 1;
        burst.3 = now;
        burst.4 |= during_rescan;
        during_rescan
    }

    fn flush_overflow_summary(&mut self) {
        let Some((reason, count, first, last, during_rescan)) = self.overflow_burst.take() else {
            return;
        };
        let window_ms = last.saturating_duration_since(first).as_millis();
        let prefixes = self.overflow_prefixes();
        let backend = self.config.counters.backend_exclusions();
        let render_paths = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|path| {
                    path.strip_prefix(&self.config.project_root)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let exclusions = render_paths(&backend.paths);
        // What the cap cost: the next candidates that lost a slot are still
        // watched, so a burst attributed to one of them explains the overflow.
        let candidates_dropped = render_paths(&backend.candidates_dropped);
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
            "watcher overflow: reason={} root={} exclusions=[{}] candidates_dropped=[{}] matcher_generation={} top_prefixes=[{}] ring_span_ms={} queue_depth={} rescan_in_progress={} count={} window_ms={}",
            reason.as_str(),
            self.config.project_root.display(),
            exclusions,
            candidates_dropped,
            backend.matcher_generation,
            prefixes,
            span_ms,
            queue_depth,
            during_rescan,
            count,
            window_ms
        );
        emit_watcher_overflow_log(line);
    }

    #[cfg(test)]
    fn push_raw_paths(&mut self, paths: Vec<PathBuf>) -> bool {
        self.push_raw_paths_with_kind(paths, false)
    }

    fn push_raw_paths_with_kind(&mut self, paths: Vec<PathBuf>, content_only: bool) -> bool {
        for path in paths {
            // Drop high-churn ignored dirs (target/, node_modules/, agent infra)
            // on the RAW path before canonicalizing. `canonicalize_watcher_path`
            // is a realpath syscall; a build floods FSEvents with hundreds of
            // thousands of target/ paths, and paying a syscall per path only to
            // discard them later pegged this single watcher thread. The full
            // filter still drops these (watcher_path_is_infra_skip), so this is a
            // pure perf short-circuit with no behavior change.
            // Operating-system metadata files are dropped here too: the check is
            // a pure name comparison, and dropping them before they enter the
            // batch keeps a lone `.DS_Store` write from arming a flush.
            if path
                .strip_prefix(&self.config.project_root)
                .is_ok_and(watcher_path_is_high_churn_infra)
                || crate::os_metadata::is_os_metadata_path(&path)
                || crate::watcher_backend::cookie::is_cookie_path(&path)
            {
                continue;
            }
            // Canonicalize at intake so the set keys (and downstream consumers)
            // see normalized paths — this is what collapses macOS /var ->
            // /private/var aliasing and matches the callgraph/semantic/search
            // cache keys. Same-file repeats within the window still dedup here;
            // the high-churn flood is already dropped above, before this syscall.
            let path = canonicalize_watcher_path(path);
            // Published before the flush window ends so that call-graph
            // answers in the meantime already know these files are changing.
            self.config
                .counters
                .note_unflushed_paths(std::iter::once(&path));
            #[cfg(feature = "test-timing-hooks")]
            note_watcher_intake_for_test(&path);
            let fresh = self.raw_paths.insert(path.clone());
            if content_only && (fresh || self.content_only_paths.contains(&path)) {
                self.content_only_paths.insert(path);
            } else {
                // A create followed by a write is still a create, even when the
                // debounce window collapses both events to one path.
                self.content_only_paths.remove(&path);
            }
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
        // Wake in time to flush a pending overflow summary, and sooner while
        // dispatch-backlogged events are waiting for room.
        let root_timeout = self
            .next_root_check
            .saturating_duration_since(Instant::now());
        let mut timeout = self.overflow_burst.as_ref().map_or(root_timeout, |burst| {
            (burst.3 + WATCHER_OVERFLOW_QUIET_INTERVAL)
                .min(burst.2 + WATCHER_OVERFLOW_MAX_INTERVAL)
                .saturating_duration_since(Instant::now())
                .min(root_timeout)
        });
        if !self.backlog.is_empty() {
            timeout = timeout.min(DISPATCH_BACKLOG_RETRY_INTERVAL);
        }
        match self.flush_deadline {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .min(timeout),
            None => timeout,
        }
    }

    fn flush_deadline_reached(&self) -> bool {
        self.flush_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn flush_pending(&mut self) -> bool {
        self.flush_pending_with_wait(Instant::now() + Duration::from_secs(2), || {
            thread::sleep(GITIGNORE_REBUILD_POLL_INTERVAL)
        })
    }

    fn flush_pending_with_wait(&mut self, deadline: Instant, pause: impl FnMut()) -> bool {
        #[cfg(feature = "test-timing-hooks")]
        if !self.raw_paths.is_empty() {
            // An idle tick has nothing to keep pending; holding it would stop
            // the thread before it takes in the event the test is waiting for.
            hold_watcher_flush_for_test(&self.shutdown);
        }
        let flushed = self.flush_raw_paths_with_wait(deadline, pause);
        // Paths kept for another window (an ignore-rule change still being
        // acknowledged) remain unflushed; everything else has been handed to
        // the dispatch channel or dropped by the filter.
        if self.raw_paths.is_empty() {
            self.config.counters.clear_unflushed_paths();
        }
        flushed
    }

    fn flush_raw_paths_with_wait(&mut self, deadline: Instant, pause: impl FnMut()) -> bool {
        if self.raw_paths.is_empty() {
            self.flush_deadline = None;
            return true;
        }

        let raw_paths = std::mem::take(&mut self.raw_paths);
        let content_only_paths = std::mem::take(&mut self.content_only_paths);
        self.flush_deadline = None;
        let initial = filter_canonical_paths(&self.config, &self.matcher, raw_paths.clone());
        let filtered = if initial.ignore_file_changed {
            let observed_generation = self.matcher_generation.load(Ordering::SeqCst);
            match self.send_dispatch(WatcherDispatchEvent::IgnoreRulesChanged {
                paths: initial.ignore_file_paths.into_iter().collect(),
            }) {
                Dispatch::Closed => return false,
                // The consumer has not taken the events ahead of these rules,
                // so it cannot acknowledge them before the deadline. A rescan
                // held behind them covers this batch under the rebuilt
                // matcher, so the batch itself need not wait.
                Dispatch::Held => {
                    return self.send_dispatch(WatcherDispatchEvent::RescanRequired(
                        RescanReason::Unknown,
                    )) != Dispatch::Closed;
                }
                Dispatch::Sent => {}
            }
            if !self.wait_for_gitignore_rebuild_until(observed_generation, deadline, pause) {
                return false;
            }
            if self.matcher_generation.load(Ordering::SeqCst) == observed_generation {
                // Keep the batch until the ignore matcher is acknowledged; a
                // lifecycle transition may have deferred the original rebuild.
                self.raw_paths.extend(raw_paths);
                self.content_only_paths.extend(content_only_paths);
                self.flush_deadline = Some(Instant::now() + WATCHER_FLUSH_WINDOW);
                return self
                    .send_dispatch(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
                    != Dispatch::Closed;
            }
            filter_canonical_paths(&self.config, &self.matcher, raw_paths)
        } else {
            initial
        };
        self.config
            .counters
            .note_paths_after_gitignore(filtered.changed.len());
        if filtered.changed.is_empty() {
            return true;
        }
        let paths = filtered.changed.into_iter().collect::<Vec<_>>();
        let content_only = paths.iter().all(|path| content_only_paths.contains(path));
        let path_count = paths.len();
        // A parent folder session follows its child repositories through
        // this watcher; it only queues the paths for its own worker.
        crate::views::parent::note_changed_paths(&paths);
        let event = if content_only {
            WatcherDispatchEvent::ContentOnlyPaths(paths)
        } else {
            WatcherDispatchEvent::Paths(paths)
        };
        if self.send_dispatch(event) == Dispatch::Closed {
            return false;
        }
        self.config.counters.note_paths_dispatched(path_count);
        true
    }

    fn wait_for_gitignore_rebuild_until(
        &mut self,
        observed_generation: u64,
        deadline: Instant,
        mut pause: impl FnMut(),
    ) -> bool {
        while !self.shutdown.load(Ordering::SeqCst)
            && self.matcher_generation.load(Ordering::SeqCst) == observed_generation
        {
            if self.project_root_was_deleted(Instant::now()) {
                let _ = self.send_dispatch_final(WatcherDispatchEvent::RootDeleted);
                return false;
            }
            if Instant::now() >= deadline {
                return true;
            }
            pause();
        }
        !self.shutdown.load(Ordering::SeqCst)
    }

    /// Hands `event` to the dispatch channel without waiting. When the channel
    /// is full, or older events are still held, the event joins the backlog
    /// and the run loop retries it every `DISPATCH_BACKLOG_RETRY_INTERVAL`.
    ///
    /// Waiting for room here used to park this thread in the channel's send,
    /// whose back-off spins and yields before each sleep: a consumer that had
    /// stopped draining (a root with no bound session keeps its watcher but
    /// runs no drains) kept the thread busy indefinitely, and raw events piled
    /// up unread behind it.
    fn send_dispatch(&mut self, event: WatcherDispatchEvent) -> Dispatch {
        if self.shutdown.load(Ordering::SeqCst) || !self.retry_backlog() {
            return Dispatch::Closed;
        }
        if self.backlog.is_empty() {
            match self.dispatch_tx.try_send(event) {
                Ok(()) => return Dispatch::Sent,
                Err(TrySendError::Disconnected(_)) => return Dispatch::Closed,
                Err(TrySendError::Full(returned)) => self.hold_event(returned),
            }
        } else {
            self.hold_event(event);
        }
        self.config
            .counters
            .set_dispatch_backlog(self.backlog.len());
        Dispatch::Held
    }

    /// Delivers the backlog and then `event`, waiting for room. Used for the
    /// last event before the thread stops (root deleted, watcher failure),
    /// which must not be left in a backlog nobody will retry. Waits by
    /// sleeping, never in the channel's spinning send. Returns false if
    /// shutdown was requested or the consumer is gone first.
    fn send_dispatch_final(&mut self, event: WatcherDispatchEvent) -> bool {
        self.backlog.push_back(event);
        loop {
            if !self.retry_backlog() {
                return false;
            }
            if self.backlog.is_empty() {
                return true;
            }
            if self.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            thread::sleep(DISPATCH_SEND_POLL_INTERVAL);
        }
    }

    /// Moves held events into the dispatch channel, oldest first, until it is
    /// full. Returns false if the consumer is gone.
    fn retry_backlog(&mut self) -> bool {
        if self.backlog.is_empty() {
            return true;
        }
        while let Some(event) = self.backlog.pop_front() {
            match self.dispatch_tx.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(returned)) => {
                    self.backlog.push_front(returned);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.backlog.clear();
                    self.config.counters.set_dispatch_backlog(0);
                    return false;
                }
            }
        }
        self.config
            .counters
            .set_dispatch_backlog(self.backlog.len());
        true
    }

    /// Adds an event to the backlog, merging it with what is already held so
    /// the backlog stays small however long the channel stays full:
    ///
    /// - changed paths join the newest held path batch (a batch is
    ///   content-only only if every merged batch was);
    /// - a rescan request replaces every held path batch, and later path
    ///   batches are dropped while it is held, because the rescan runs after
    ///   them and reads the files as they are then;
    /// - consecutive ignore-rule changes are merged into one.
    ///
    /// Order is otherwise kept. Held paths beyond
    /// `WATCHER_DISPATCH_BACKLOG_MAX_PATHS` are replaced by a rescan request,
    /// logged and counted as a buffer overflow.
    fn hold_event(&mut self, event: WatcherDispatchEvent) {
        let rescan_held = self
            .backlog
            .iter()
            .any(|held| matches!(held, WatcherDispatchEvent::RescanRequired(_)));
        match event {
            WatcherDispatchEvent::Paths(paths) | WatcherDispatchEvent::ContentOnlyPaths(paths)
                if rescan_held =>
            {
                drop(paths);
            }
            WatcherDispatchEvent::Paths(paths) => self.hold_paths(paths, false),
            WatcherDispatchEvent::ContentOnlyPaths(paths) => self.hold_paths(paths, true),
            WatcherDispatchEvent::RescanRequired(reason) => {
                // Losing the paths before a held cookie also loses its proof.
                self.backlog.retain(|held| {
                    !is_path_batch(held) && !matches!(held, WatcherDispatchEvent::CookieObserved(_))
                });
                if !rescan_held {
                    self.backlog
                        .push_back(WatcherDispatchEvent::RescanRequired(reason));
                }
            }
            WatcherDispatchEvent::CookieObserved(path) => {
                if !rescan_held {
                    self.backlog
                        .push_back(WatcherDispatchEvent::CookieObserved(path));
                }
            }
            WatcherDispatchEvent::IgnoreRulesChanged { paths } => match self.backlog.back_mut() {
                Some(WatcherDispatchEvent::IgnoreRulesChanged { paths: held }) => {
                    merge_paths(held, paths);
                }
                _ => self
                    .backlog
                    .push_back(WatcherDispatchEvent::IgnoreRulesChanged { paths }),
            },
            event @ (WatcherDispatchEvent::RootDeleted | WatcherDispatchEvent::Error(_)) => {
                self.backlog.push_back(event);
            }
        }
    }

    fn hold_paths(&mut self, paths: Vec<PathBuf>, content_only: bool) {
        match self.backlog.back_mut() {
            Some(WatcherDispatchEvent::Paths(held)) => merge_paths(held, paths),
            Some(held @ WatcherDispatchEvent::ContentOnlyPaths(_)) => {
                let WatcherDispatchEvent::ContentOnlyPaths(held_paths) = held else {
                    unreachable!()
                };
                let mut merged = std::mem::take(held_paths);
                merge_paths(&mut merged, paths);
                *held = if content_only {
                    WatcherDispatchEvent::ContentOnlyPaths(merged)
                } else {
                    WatcherDispatchEvent::Paths(merged)
                };
            }
            _ => self.backlog.push_back(if content_only {
                WatcherDispatchEvent::ContentOnlyPaths(paths)
            } else {
                WatcherDispatchEvent::Paths(paths)
            }),
        }
        let held_paths: usize = self
            .backlog
            .iter()
            .map(|held| match held {
                WatcherDispatchEvent::Paths(paths)
                | WatcherDispatchEvent::ContentOnlyPaths(paths) => paths.len(),
                _ => 0,
            })
            .sum();
        if held_paths > WATCHER_DISPATCH_BACKLOG_MAX_PATHS {
            // Too many to keep: fall back to the overflow recovery, a rescan,
            // logged and counted like an operating-system event overflow. A
            // rescan already running is told to run again instead.
            let during_rescan = self.log_overflow(RescanReason::BufferOverflow);
            self.backlog.retain(|held| {
                !is_path_batch(held) && !matches!(held, WatcherDispatchEvent::CookieObserved(_))
            });
            if !during_rescan {
                self.backlog.push_back(WatcherDispatchEvent::RescanRequired(
                    RescanReason::BufferOverflow,
                ));
            }
        }
    }
}

impl Drop for WatcherFilterThread {
    fn drop(&mut self) {
        // Held events end with the thread; stop reporting them as pending.
        self.config.counters.set_dispatch_backlog(0);
    }
}

fn is_path_batch(event: &WatcherDispatchEvent) -> bool {
    matches!(
        event,
        WatcherDispatchEvent::Paths(_) | WatcherDispatchEvent::ContentOnlyPaths(_)
    )
}

/// Adds `more` to `held`, keeping each path once.
fn merge_paths(held: &mut Vec<PathBuf>, more: Vec<PathBuf>) {
    held.extend(more);
    held.sort_unstable();
    held.dedup();
}

/// The gate file a test names in `AFT_TEST_WATCHER_INTAKE_GATE_FILE`, if any.
#[cfg(feature = "test-timing-hooks")]
fn watcher_intake_gate_for_test() -> Option<&'static Path> {
    static GATE: OnceLock<Option<PathBuf>> = OnceLock::new();
    GATE.get_or_init(|| std::env::var_os("AFT_TEST_WATCHER_INTAKE_GATE_FILE").map(PathBuf::from))
        .as_deref()
}

/// Test seam: while the gate file exists, the watcher thread holds every raw
/// event before taking it in, so a test can question the daemon at a moment
/// when it cannot know about an edit yet. Removing the file releases the held
/// events in order.
#[cfg(feature = "test-timing-hooks")]
fn hold_watcher_intake_for_test(shutdown: &AtomicBool) {
    let Some(gate) = watcher_intake_gate_for_test() else {
        return;
    };
    while gate.exists() && !shutdown.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Test seam: while `<gate>.flush-hold` exists, the watcher thread keeps taken-in
/// paths in its flush window instead of handing them on, so an edit it has
/// received stays pending until the test has asked its question.
#[cfg(feature = "test-timing-hooks")]
fn hold_watcher_flush_for_test(shutdown: &AtomicBool) {
    let Some(gate) = watcher_intake_gate_for_test() else {
        return;
    };
    let mut hold = gate.as_os_str().to_owned();
    hold.push(".flush-hold");
    let hold = PathBuf::from(hold);
    while hold.exists() && !shutdown.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Test seam: append each path the watcher thread takes in to `<gate>.taken`,
/// one per line, so a test can wait until the daemon has received an edit.
#[cfg(feature = "test-timing-hooks")]
fn note_watcher_intake_for_test(path: &Path) {
    use std::io::Write;
    let Some(gate) = watcher_intake_gate_for_test() else {
        return;
    };
    let mut taken = gate.as_os_str().to_owned();
    taken.push(".taken");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(taken))
    {
        let _ = writeln!(file, "{}", path.display());
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
    #[test]
    fn watcher_cookie_bypasses_ignore_and_flushes_mixed_paths_before_ack() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source.rs");
        let cookie = root.join(".aft-cookie-123-nonce");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&cookie, b"").unwrap();
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
        builder.add_line(None, ".aft-cookie-*").unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let (tx, rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.clone(), None),
            matcher,
            Arc::new(AtomicU64::new(0)),
            tx,
            Arc::clone(&shutdown),
        );
        let worker = thread::spawn(move || filter.run(raw_rx));
        raw_tx
            .send(Ok(notify::Event::new(notify::EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(source.clone())
            .add_path(cookie.clone())))
            .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            WatcherDispatchEvent::Paths(vec![source])
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            WatcherDispatchEvent::CookieObserved(cookie.clone())
        );
        assert!(!cookie.exists());
        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        worker.join().unwrap();
    }

    #[test]
    fn watcher_cookie_unlinks_on_overflow_but_only_create_can_ack() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (filter, _rx, _) = filter_with_full_dispatch_channel(&root);
        let cookie = root.join(".aft-cookie-123-nonce");
        for kind in [
            notify::EventKind::Remove(notify::event::RemoveKind::File),
            notify::EventKind::Any,
        ] {
            std::fs::write(&cookie, b"").unwrap();
            let mut event = notify::Event::new(kind).add_path(cookie.clone());
            assert!(filter.take_cookie_paths(&mut event).is_empty());
            assert!(event.paths.is_empty());
            assert!(!cookie.exists());
        }
        std::fs::write(&cookie, b"").unwrap();
        let mut event =
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(cookie.clone())
                .set_flag(notify::event::Flag::Rescan);
        assert!(filter.take_cookie_paths(&mut event).is_empty());
        assert!(event.need_rescan());
        assert!(!cookie.exists());
        let nested = root.join("nested/.aft-cookie-123-nonce");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, b"not a root control").unwrap();
        let mut event =
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(nested.clone());
        assert!(filter.take_cookie_paths(&mut event).is_empty());
        assert!(nested.exists());
    }

    #[test]
    fn watcher_cookie_backlog_preserves_barriers_and_drops_them_with_lost_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (mut filter, _rx, _) = filter_with_full_dispatch_channel(&root);
        filter.send_dispatch(WatcherDispatchEvent::Paths(vec![root.join("a.rs")]));
        filter.send_dispatch(WatcherDispatchEvent::CookieObserved(
            root.join(".aft-cookie-1-nonce"),
        ));
        filter.send_dispatch(WatcherDispatchEvent::Paths(vec![root.join("b.rs")]));
        assert_eq!(
            filter.backlog.len(),
            3,
            "paths must not merge across the cookie"
        );
        filter.send_dispatch(WatcherDispatchEvent::RescanRequired(
            RescanReason::BufferOverflow,
        ));
        assert_eq!(filter.backlog.len(), 1);
        assert!(matches!(
            filter.backlog.front(),
            Some(WatcherDispatchEvent::RescanRequired(_))
        ));
        filter.send_dispatch(WatcherDispatchEvent::CookieObserved(
            root.join(".aft-cookie-1-next"),
        ));
        assert_eq!(
            filter.backlog.len(),
            1,
            "a held rescan cannot certify a cookie"
        );
    }

    #[test]
    fn issue645_nested_git_and_local_manifests_get_kernel_exclusions() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for repo in ["service-a", "group/service-b"] {
            let repo = root.join(repo);
            std::fs::create_dir_all(repo.join(".git/worktrees/linked")).unwrap();
            std::fs::create_dir_all(repo.join("node_modules")).unwrap();
            std::fs::write(repo.join("package.json"), "{}").unwrap();
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
        builder.add_line(None, "node_modules/").unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let plan = derive_exclusion_plan(&root, &matcher, None, WatcherExclusionCoverage::Subtree);
        for repo in ["service-a", "group/service-b"] {
            for name in [".git", "node_modules"] {
                let path = root.join(repo).join(name);
                let candidate = plan
                    .selected
                    .iter()
                    .find(|entry| entry.path == path)
                    .unwrap_or_else(|| panic!("missing kernel exclusion: {}", path.display()));
                assert_eq!(candidate.source, WatcherExclusionSource::Ecosystem);
            }
        }
    }

    #[test]
    fn issue645_ecosystem_gates_use_the_containing_repository() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for name in ["node_modules", "target", ".venv"] {
            std::fs::create_dir_all(root.join("repo").join(name)).unwrap();
        }
        for manifest in ["package.json", "Cargo.toml", "pyproject.toml"] {
            std::fs::write(root.join("repo").join(manifest), "").unwrap();
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
        for name in ["node_modules/", "target/", ".venv/"] {
            builder.add_line(None, name).unwrap();
        }
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let plan = derive_exclusion_plan(&root, &matcher, None, WatcherExclusionCoverage::Subtree);
        for name in ["node_modules", "target", ".venv"] {
            let path = root.join("repo").join(name);
            let candidate = plan
                .selected
                .iter()
                .find(|entry| entry.path == path)
                .unwrap();
            assert_eq!(
                candidate.source,
                WatcherExclusionSource::Ecosystem,
                "{} needs its local manifest",
                path.display()
            );
        }
    }

    #[test]
    fn issue645_overflow_burst_logs_one_summary() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (tx, _rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.clone(), None),
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        for _ in 0..12 {
            raw_tx
                .send(Ok(notify::Event::new(notify::EventKind::Other)
                    .set_flag(notify::event::Flag::Rescan)))
                .unwrap();
        }
        drop(raw_tx);
        filter.run(raw_rx);
        let lines = take_watcher_overflow_logs_for_test()
            .into_iter()
            .filter(|line| line.contains(&format!("root={}", root.display())))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "one summary per overflow burst: {lines:?}");
        assert!(lines[0].contains("count=12"), "{}", lines[0]);
        assert!(lines[0].contains("window_ms="), "{}", lines[0]);
    }

    #[test]
    fn issue645_backend_overflow_preserves_buffered_ignore_relevance() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (tx, _rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let config = WatcherFilterConfig::new(root.clone(), None);
        let counters = config.counters.clone();
        let mut filter = WatcherFilterThread::new(
            config,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        filter.raw_paths.insert(root.join(".gitignore"));
        raw_tx
            .send(Ok(notify::Event::new(notify::EventKind::Other)
                .add_path(root.join("repo/.git/objects/pack"))
                .set_info("rescan: user dropped")
                .set_flag(notify::event::Flag::Rescan)))
            .unwrap();
        drop(raw_tx);
        filter.run(raw_rx);
        assert!(filter.raw_paths.is_empty());
        assert!(
            counters.take_overflow_ignore_relevance(),
            "discarding a buffered .gitignore edit must require an ignore scan"
        );
    }

    #[test]
    fn issue645_sustained_overflow_flushes_at_maximum_interval() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (tx, _rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.clone(), None),
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let now = Instant::now();
        filter.overflow_burst = Some((
            RescanReason::UserDropped,
            12,
            now - Duration::from_secs(11),
            now,
            false,
        ));
        assert!(
            filter.next_recv_timeout().is_zero(),
            "a continuous flood must not postpone the summary deadline"
        );
        raw_tx
            .send(Ok(
                notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
            ))
            .unwrap();
        drop(raw_tx);
        filter.run_until_stopped(raw_rx);
        let (_, count, first, _, _) = filter.overflow_burst.unwrap();
        assert_eq!(count, 1, "the ongoing flood must start a new burst");
        assert!(first >= now);
    }

    #[test]
    fn issue645_backend_overflow_paths_do_not_bound_loss_scope() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("source.rs"), "fn marker() {}\n").unwrap();
        for path in [
            "source.rs",
            "a/.git/objects/pack",
            "b/.git/worktrees/linked/HEAD",
            "a/.git/refs/heads/main",
            ".gitignore",
            "a/.aftignore",
            "a/.git/info/exclude",
            "a/.git",
            "a/src",
        ] {
            for info in [
                "rescan: user dropped",
                "rescan: kernel dropped",
                "rescan: buffer overflow",
                "",
            ] {
                let (tx, _rx) = watcher_dispatch_channel();
                let (raw_tx, raw_rx) = mpsc::channel();
                let config = WatcherFilterConfig::new(root.clone(), None);
                let counters = config.counters.clone();
                let mut filter = WatcherFilterThread::new(
                    config,
                    Arc::new(RwLock::new(None)),
                    Arc::new(AtomicU64::new(0)),
                    tx,
                    Arc::new(AtomicBool::new(false)),
                );
                raw_tx
                    .send(Ok(notify::Event::new(notify::EventKind::Other)
                        .add_path(root.join(path))
                        .set_info(info)
                        .set_flag(notify::event::Flag::Rescan)))
                    .unwrap();
                drop(raw_tx);
                filter.run(raw_rx);
                assert!(counters.take_overflow_ignore_relevance(), "{path}: {info}");
            }
        }
    }

    #[test]
    fn content_only_batches_preserve_kind_and_topology_dominates_coalescing() {
        use super::*;
        use notify::event::{CreateKind, DataChange, ModifyKind, RenameMode};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source.py");
        std::fs::write(&source, "print(0)").unwrap();
        let data = notify::EventKind::Modify(ModifyKind::Data(DataChange::Content));
        let create = notify::EventKind::Create(CreateKind::File);
        let rename = notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        let ambiguous = notify::EventKind::Modify(ModifyKind::Any);
        for (kinds, expected_content_only) in [
            (vec![data, data], true),
            (vec![create, data], false),
            (vec![data, create], false),
            (vec![rename, data], false),
            (vec![ambiguous, data], false),
        ] {
            let (tx, rx) = watcher_dispatch_channel();
            let matcher = Arc::new(RwLock::new(None));
            let mut filter = WatcherFilterThread::new(
                WatcherFilterConfig::new(root.clone(), None),
                matcher,
                Arc::new(AtomicU64::new(0)),
                tx,
                Arc::new(AtomicBool::new(false)),
            );
            for kind in kinds {
                assert!(filter.push_raw_paths_with_kind(
                    vec![source.clone()],
                    watcher_event_is_content_only(&kind)
                ));
            }
            assert!(filter.flush_pending());
            let expected = if expected_content_only {
                WatcherDispatchEvent::ContentOnlyPaths(vec![source.clone()])
            } else {
                WatcherDispatchEvent::Paths(vec![source.clone()])
            };
            assert_eq!(rx.try_recv().unwrap(), expected);
        }
    }

    #[test]
    fn matcher_snapshot_releases_read_lock_before_path_work() {
        let directory = tempfile::tempdir().unwrap();
        let matcher = std::sync::Arc::new(std::sync::RwLock::new(Some(std::sync::Arc::new(
            ignore::gitignore::GitignoreBuilder::new(directory.path())
                .build()
                .unwrap(),
        ))));
        super::with_matcher_snapshot(&matcher, |snapshot| {
            assert_eq!(snapshot.unwrap().path(), directory.path());
            let mut writer = matcher
                .try_write()
                .expect("path work must not hold the matcher lock");
            *writer = None;
            assert_eq!(snapshot.unwrap().path(), directory.path());
        });
    }

    #[test]
    fn watcher_exclusion_walk_stops_after_cancellation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("source/nested")).unwrap();
        let matcher = std::sync::Arc::new(std::sync::RwLock::new(None));
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        super::EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        let plan = super::derive_exclusion_plan_cancellable(
            root.path(),
            &matcher,
            None,
            crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
            Some(&cancelled),
        );
        assert!(plan.selected.is_empty());
        super::EXCLUSION_TEST_OPENS.with(|opens| {
            assert!(
                opens.borrow().is_empty(),
                "cancelled exclusion walk must not open directories"
            );
        });
    }

    #[test]
    fn lexical_matcher_preserves_directory_rules_without_stat() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root.path());
        builder.add_line(None, "generated/").unwrap();
        builder.add_line(None, "*.tmp").unwrap();
        let matcher = builder.build().unwrap();
        let generated = root.path().join("generated");
        assert!(watcher_directory_is_ignored(Some(&matcher), &generated));
        assert!(!watcher_path_is_ignored(Some(&matcher), &generated, false));
        assert!(watcher_path_is_ignored(
            Some(&matcher),
            &generated.join("source.rs"),
            false
        ));
        assert!(watcher_path_is_ignored(
            Some(&matcher),
            Path::new("missing.tmp"),
            false
        ));
        assert!(!watcher_path_is_ignored(
            Some(&matcher),
            &root.path().join("source.rs"),
            false
        ));
    }

    #[test]
    fn expired_generation_wait_does_not_poll_forever() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _rx) = crossbeam_channel::bounded(4);
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.path().to_path_buf(), None),
            Arc::new(std::sync::RwLock::new(None)),
            generation.clone(),
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let mut polls = 0;
        assert!(
            filter.wait_for_gitignore_rebuild_until(1, Instant::now(), || {
                polls += 1;
                generation.store(2, Ordering::SeqCst);
            })
        );
        assert_eq!(
            polls, 0,
            "expired acknowledgement wait must return to the event loop"
        );
        let ignore_file = root.path().join(".gitignore");
        std::fs::write(&ignore_file, "ignored.rs\n").unwrap();
        let source = root.path().join("changed.rs");
        std::fs::write(&source, "fn changed() {}\n").unwrap();
        let (tx, rx) = crossbeam_channel::bounded(4);
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.path().to_path_buf(), None),
            Arc::new(std::sync::RwLock::new(None)),
            generation,
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        filter.raw_paths.extend([ignore_file, source.clone()]);
        assert!(filter
            .flush_pending_with_wait(Instant::now(), || panic!("expired wait must not sleep")));
        assert!(matches!(
            rx.try_recv().unwrap(),
            WatcherDispatchEvent::IgnoreRulesChanged { .. }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            WatcherDispatchEvent::RescanRequired(_)
        ));
        assert!(rx.try_recv().is_err());
        assert!(filter.raw_paths.contains(&source));
        assert!(filter.flush_deadline.is_some());
    }

    /// A watcher thread whose dispatch channel is full, built over `root`.
    /// The channel holds one `earlier.rs` batch that nobody receives.
    fn filter_with_full_dispatch_channel(
        root: &Path,
    ) -> (
        WatcherFilterThread,
        crossbeam_channel::Receiver<WatcherDispatchEvent>,
        Arc<AtomicBool>,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.try_send(WatcherDispatchEvent::Paths(vec![root.join("earlier.rs")]))
            .unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.to_path_buf(), None),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
            tx,
            Arc::clone(&shutdown),
        );
        (filter, rx, shutdown)
    }

    /// Flushing into a full dispatch channel must not park the watcher thread
    /// in the channel's send (whose back-off spins and yields): the batches
    /// are held, merged into one, and handed on once the consumer catches up.
    #[test]
    fn watcher_full_dispatch_channel_holds_and_merges_batches_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }
        let (mut filter, rx, shutdown) = filter_with_full_dispatch_channel(&root);
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config::default(),
        );
        ctx.update_config(|config| config.project_root = Some(root.clone()));
        *ctx.watcher_rx().lock() = Some(rx.clone());
        let batches_root = root.clone();
        let flusher = thread::spawn(move || {
            for batch in [["a.rs", "b.rs"], ["b.rs", "c.rs"]] {
                assert!(filter.push_raw_paths(batch.map(|name| batches_root.join(name)).to_vec()));
                assert!(filter.flush_pending(), "the watcher thread stopped");
            }
            filter
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !flusher.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let finished = flusher.is_finished();
        // Release a blocked flush so a failing run ends instead of hanging.
        shutdown.store(true, Ordering::SeqCst);
        let mut filter = flusher.join().unwrap();
        assert!(
            finished,
            "flushing into a full dispatch channel blocked the watcher thread"
        );
        shutdown.store(false, Ordering::SeqCst);
        let merged = WatcherDispatchEvent::Paths(
            ["a.rs", "b.rs", "c.rs"]
                .map(|name| root.join(name))
                .to_vec(),
        );
        assert_eq!(filter.backlog, VecDeque::from([merged.clone()]));
        assert_eq!(filter.config.counters.dispatch_backlog(), 1);

        // The consumer catches up: the held batch follows the earlier one.
        assert_eq!(
            rx.try_recv().unwrap(),
            WatcherDispatchEvent::Paths(vec![root.join("earlier.rs")])
        );
        assert!(rx.is_empty());
        assert!(
            ctx.watcher_query_has_pending_changes(),
            "held events remain pending even after the channel empties"
        );
        assert!(filter.retry_backlog());
        assert_eq!(filter.config.counters.dispatch_backlog(), 0);
        assert!(
            ctx.watcher_query_has_pending_changes(),
            "dispatched events remain pending after the backlog clears"
        );
        assert_eq!(rx.try_recv().unwrap(), merged);
        assert!(filter.backlog.is_empty());
        assert_eq!(filter.config.counters.dispatch_backlog(), 0);
        assert!(!ctx.watcher_query_has_pending_changes());
    }

    /// Held events keep their order and collapse instead of growing: a rescan
    /// replaces the path batches before it and covers those after it, and
    /// more held paths than the backlog keeps fall back to a rescan, logged as
    /// a buffer overflow like any other watcher overflow.
    #[test]
    fn watcher_dispatch_backlog_collapses_into_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (mut filter, _rx, _shutdown) = filter_with_full_dispatch_channel(&root);
        let batch = |count: usize, offset: usize| {
            (offset..offset + count)
                .map(|index| root.join(format!("file-{index}.rs")))
                .collect::<Vec<_>>()
        };

        let ignore = WatcherDispatchEvent::IgnoreRulesChanged {
            paths: vec![root.join(".gitignore")],
        };
        assert_eq!(filter.send_dispatch(ignore.clone()), Dispatch::Held);
        assert_eq!(
            filter.send_dispatch(WatcherDispatchEvent::ContentOnlyPaths(batch(2, 0))),
            Dispatch::Held
        );
        assert_eq!(
            filter.send_dispatch(WatcherDispatchEvent::RescanRequired(
                RescanReason::UserDropped
            )),
            Dispatch::Held
        );
        assert_eq!(
            filter.send_dispatch(WatcherDispatchEvent::Paths(batch(2, 2))),
            Dispatch::Held
        );
        assert_eq!(
            filter.backlog,
            VecDeque::from([
                ignore.clone(),
                WatcherDispatchEvent::RescanRequired(RescanReason::UserDropped),
            ]),
            "a held rescan must cover the path batches around it"
        );

        let (mut filter, _rx, _shutdown) = filter_with_full_dispatch_channel(&root);
        let mut held = 0;
        while held <= WATCHER_DISPATCH_BACKLOG_MAX_PATHS {
            let event = WatcherDispatchEvent::Paths(batch(WATCHER_MAX_BATCH_PATHS, held));
            assert_eq!(filter.send_dispatch(event), Dispatch::Held);
            held += WATCHER_MAX_BATCH_PATHS;
            assert!(filter.backlog.len() <= 1, "held batches were not merged");
        }
        assert_eq!(
            filter.backlog,
            VecDeque::from([WatcherDispatchEvent::RescanRequired(
                RescanReason::BufferOverflow
            )]),
            "an overflowing backlog must fall back to a rescan"
        );
        // Overflows are logged as one summary per burst, written when the
        // burst ends; end it here instead of waiting for the quiet period.
        filter.flush_overflow_summary();
        let root_label = format!("root={}", root.display());
        assert!(
            take_watcher_overflow_logs_for_test()
                .iter()
                .any(|line| line.contains("reason=buffer_overflow") && line.contains(&root_label)),
            "the backlog overflow was not logged"
        );
    }

    /// CPU time the calling thread has used.
    #[cfg(unix)]
    fn thread_cpu_time() -> Duration {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) },
            0
        );
        Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
    }

    /// The live shape of the problem: a root whose dispatch channel nobody
    /// drains keeps receiving file events. The watcher thread must keep
    /// taking raw events in (they used to pile up unread while it waited in
    /// the channel's send) and must stay cheap while it waits.
    #[cfg(unix)]
    #[test]
    fn watcher_full_dispatch_channel_keeps_intake_running_cheaply() {
        const EVENTS: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (mut filter, _rx, shutdown) = filter_with_full_dispatch_channel(&root);
        let counters = Arc::clone(&filter.config.counters);
        let raw_before = counters.snapshot().raw_events_total;
        let (raw_tx, raw_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let cpu_start = thread_cpu_time();
            filter.run(raw_rx);
            thread_cpu_time() - cpu_start
        });
        let started = Instant::now();
        for index in 0..EVENTS {
            let path = root.join(format!("burst-{index}.rs"));
            std::fs::write(&path, "fn f() {}\n").unwrap();
            raw_tx
                .send(Ok(
                    notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path)
                ))
                .unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        // Let the last flush window end, then give the thread time to idle.
        thread::sleep(Duration::from_millis(1_000));
        let taken_in = counters.snapshot().raw_events_total - raw_before;
        let elapsed = started.elapsed();
        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        let cpu = handle.join().unwrap();
        eprintln!(
            "full dispatch channel: {taken_in} of {EVENTS} raw events taken in; watcher thread CPU {:?} over {:?}",
            cpu, elapsed
        );
        assert_eq!(
            taken_in, EVENTS as u64,
            "raw events piled up unread while the dispatch channel was full"
        );
        assert!(
            cpu < elapsed / 4,
            "the watcher thread stayed busy while the dispatch channel was full: {cpu:?} over {elapsed:?}"
        );
    }

    use super::*;
    use ignore::gitignore::GitignoreBuilder;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind,
    };
    use notify::EventKind;
    use std::process::Command;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn exclusion_tempdir() -> TempDir {
        tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }

    #[test]
    fn excluded_topology_burst_stats_follow_distinct_directories() {
        const EVENTS: usize = 512;
        for _ in 0..2 {
            let root = TempDir::new().unwrap();
            let excluded = root.path().join("packages/pkg/target");
            fs::create_dir_all(&excluded).unwrap();
            let paths: Vec<_> = (0..EVENTS)
                .map(|index| excluded.join(format!("object-{index}.o")))
                .collect();
            let mut state = WatcherExclusionPaths::new(
                root.path(),
                &[WatcherExclusion {
                    path: excluded.clone(),
                    source: WatcherExclusionSource::Ecosystem,
                }],
            );
            EXCLUSION_TEST_STAT_PATHS.with(|paths| paths.borrow_mut().clear());
            let events: Vec<_> = paths
                .into_iter()
                .map(|path| notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path))
                .collect();
            assert!(state.refresh_events(&events).is_empty());
            let stats = EXCLUSION_TEST_STAT_PATHS.with(|paths| paths.borrow().clone());
            eprintln!(
                "root={} events={EVENTS} lstat={}",
                root.path().display(),
                stats.len()
            );
            assert_eq!(
                stats.len(),
                3,
                "only the distinct covering directories need lstat"
            );
            assert_eq!(
                stats,
                vec![
                    root.path().join("packages"),
                    root.path().join("packages/pkg"),
                    excluded.clone()
                ]
            );
            assert_eq!(state.paths(), vec![excluded]);
        }
    }

    #[test]
    fn topology_batch_reuses_component_probes_without_an_exclusion() {
        let root = TempDir::new().unwrap();
        let directory = root.path().join("src/nested");
        fs::create_dir_all(&directory).unwrap();
        let mut state = WatcherExclusionPaths::new(root.path(), &[]);
        let events: Vec<_> = (0..128)
            .map(|index| {
                notify::Event::new(EventKind::Create(CreateKind::File))
                    .add_path(directory.join(format!("source-{index}.rs")))
            })
            .collect();
        for _ in 0..2 {
            EXCLUSION_TEST_STAT_PATHS.with(|paths| paths.borrow_mut().clear());
            assert!(state.refresh_events(&events).is_empty());
            let stats = EXCLUSION_TEST_STAT_PATHS.with(|paths| paths.borrow().clone());
            assert_eq!(
                stats.len(),
                130,
                "two directories plus each unknown leaf, not three probes per event"
            );
            assert_eq!(stats.iter().filter(|path| **path == directory).count(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn topology_batch_cache_expires_for_child_parent_and_pathless_events() {
        let root = exclusion_tempdir();
        let outside = TempDir::new().unwrap();
        let parent = root.path().join("parent");
        let excluded = parent.join("target");
        fs::create_dir_all(&excluded).unwrap();
        let mut state = WatcherExclusionPaths::new(
            root.path(),
            &[WatcherExclusion {
                path: excluded.clone(),
                source: WatcherExclusionSource::Ecosystem,
            }],
        );
        let child_event = notify::Event::new(EventKind::Create(CreateKind::File))
            .add_path(excluded.join("object.o"));
        assert!(state.refresh_event(&child_event).is_empty());
        fs::remove_dir(&excluded).unwrap();
        std::os::unix::fs::symlink(outside.path(), &excluded).unwrap();
        assert_eq!(state.refresh_event(&child_event), vec![excluded.clone()]);
        fs::remove_file(&excluded).unwrap();
        assert_eq!(
            state.refresh_event(
                &notify::Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
                    .add_path(parent.clone())
            ),
            vec![excluded.clone()]
        );
        fs::create_dir(&excluded).unwrap();
        assert!(state.refresh_event(&child_event).is_empty());
        fs::remove_dir(&excluded).unwrap();
        std::os::unix::fs::symlink(outside.path(), &excluded).unwrap();
        assert_eq!(state.refresh_event(&child_event), vec![excluded.clone()]);
        fs::remove_file(&excluded).unwrap();
        fs::create_dir(&excluded).unwrap();
        assert_eq!(
            state.refresh_event(&notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)),
            vec![excluded.clone()]
        );
        assert_eq!(state.paths(), vec![excluded]);
    }

    #[test]
    fn root_existence_probes_follow_poll_deadlines_not_event_count() {
        let root = TempDir::new().unwrap();
        let (dispatch_tx, _dispatch_rx) = watcher_dispatch_channel();
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.path().to_path_buf(), None),
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
            dispatch_tx,
            Arc::new(AtomicBool::new(false)),
        );
        let now = Instant::now();
        WATCHER_TEST_ROOT_PROBES.with(|calls| calls.set(0));
        for _ in 0..10_000 {
            assert!(!filter.project_root_was_deleted(now));
        }
        WATCHER_TEST_ROOT_PROBES.with(|calls| assert_eq!(calls.get(), 1));
        fs::remove_dir(root.path()).unwrap();
        assert!(filter.project_root_was_deleted(now + ROOT_DELETED_CHECK_INTERVAL));
        WATCHER_TEST_ROOT_PROBES.with(|calls| assert_eq!(calls.get(), 2));
        filter.next_root_check = Instant::now();
        assert!(
            filter.next_recv_timeout().is_zero(),
            "continuous intake cannot postpone the next root check"
        );
    }

    #[test]
    fn data_modify_burst_does_not_probe_external_paths() {
        let root = TempDir::new().unwrap();
        let directory = root.path().join("a/b/c/d/e/f/g/h/i/j/k");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("source.rs");
        fs::write(&path, "pub fn source() {}\n").unwrap();
        let mut state = WatcherExclusionPaths::new(root.path(), &[]);
        let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Content,
        )))
        .add_path(path);
        EXCLUSION_TEST_LSTATS.with(|calls| calls.set(0));
        EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| calls.set(0));
        for _ in 0..2_000 {
            assert!(state.refresh_event(&event).is_empty());
        }
        let calls = (
            EXCLUSION_TEST_LSTATS.with(|calls| calls.get()),
            EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| calls.get()),
        );
        assert_eq!(calls, (0, 0), "data writes must not lstat or classify");
    }

    #[test]
    fn metadata_and_access_events_do_not_probe_external_paths() {
        use notify::event::{AccessKind, MetadataKind, ModifyKind};
        let root = TempDir::new().unwrap();
        let mut state = WatcherExclusionPaths::new(root.path(), &[]);
        EXCLUSION_TEST_LSTATS.with(|calls| calls.set(0));
        EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| calls.set(0));
        for kind in [
            notify::EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            notify::EventKind::Access(AccessKind::Any),
        ] {
            let event = notify::Event::new(kind).add_path(root.path().join("source.rs"));
            assert!(state.refresh_event(&event).is_empty());
        }
        EXCLUSION_TEST_LSTATS.with(|calls| assert_eq!(calls.get(), 0));
        EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| assert_eq!(calls.get(), 0));
    }

    #[test]
    fn topology_refresh_and_matcher_replacement_reuse_the_root_boundary() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source.rs");
        fs::write(&source, "pub fn source() {}\n").unwrap();
        EXCLUSION_TEST_LSTATS.with(|calls| calls.set(0));
        let mut state = WatcherExclusionPaths::new(root.path(), &[]);
        EXCLUSION_TEST_LSTATS.with(|calls| assert_eq!(calls.get(), 1));
        state.replace(&[]);
        EXCLUSION_TEST_LSTATS.with(|calls| assert_eq!(calls.get(), 1));
        EXCLUSION_TEST_LSTATS.with(|calls| calls.set(0));
        EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| calls.set(0));
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(source);
        for _ in 0..100 {
            assert!(state.refresh_event(&event).is_empty());
        }
        // Each create probes its file once, never the already-bound root.
        EXCLUSION_TEST_LSTATS.with(|calls| assert_eq!(calls.get(), 100));
        EXCLUSION_TEST_CLASSIFICATIONS.with(|calls| assert_eq!(calls.get(), 100));
    }

    #[cfg(unix)]
    #[test]
    fn topology_and_pathless_rescan_events_refresh_external_paths() {
        use notify::event::{CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};
        for kind in [
            notify::EventKind::Create(CreateKind::Folder),
            notify::EventKind::Create(CreateKind::File),
            notify::EventKind::Remove(RemoveKind::File),
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            notify::EventKind::Other,
        ] {
            let root = exclusion_tempdir();
            let external = TempDir::new().unwrap();
            let foo = root.path().join("foo");
            std::os::unix::fs::symlink(external.path(), &foo).unwrap();
            let mut state = WatcherExclusionPaths::new(
                root.path(),
                &[WatcherExclusion {
                    path: foo.clone(),
                    source: WatcherExclusionSource::External,
                }],
            );
            fs::remove_file(&foo).unwrap();
            fs::create_dir(&foo).unwrap();
            let event = if kind == notify::EventKind::Other {
                notify::Event::new(kind).set_flag(Flag::Rescan)
            } else {
                notify::Event::new(kind).add_path(foo.clone())
            };
            assert_eq!(state.refresh_event(&event), vec![foo]);
            assert!(state.paths().is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn external_refresh_preserves_lasting_exclusions_and_rechecks_renamed_parents() {
        let root = exclusion_tempdir();
        let external = TempDir::new().unwrap();
        let parent = root.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let linked = parent.join("foo");
        std::os::unix::fs::symlink(external.path(), &linked).unwrap();
        let lasting: Vec<_> = [
            WatcherExclusionSource::Ecosystem,
            WatcherExclusionSource::Observed,
            WatcherExclusionSource::Gitignore,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, source)| WatcherExclusion {
            path: root.path().join(format!("lasting-{index}")),
            source,
        })
        .collect();
        let mut entries = lasting.clone();
        entries.push(WatcherExclusion {
            path: linked.clone(),
            source: WatcherExclusionSource::External,
        });
        let mut state = WatcherExclusionPaths::new(root.path(), &entries);
        fs::remove_file(&linked).unwrap();
        fs::create_dir(&linked).unwrap();
        let mut touched = watcher_exclusion_paths(&lasting);
        touched.push(parent);
        assert!(state.refresh_touched(&touched));
        assert_eq!(state.paths(), watcher_exclusion_paths(&lasting));

        fs::remove_dir(&linked).unwrap();
        std::os::unix::fs::symlink(external.path(), &linked).unwrap();
        EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        assert!(state.refresh_touched(&[linked.join("not-probed/deep")]));
        assert!(state.paths().contains(&linked));
        EXCLUSION_TEST_OPENS.with(|opens| assert!(opens.borrow().is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn external_refresh_keeps_other_device_until_it_becomes_local() {
        let root = exclusion_tempdir();
        let mounted = root.path().join("mounted");
        fs::create_dir_all(mounted.join("deep")).unwrap();
        let mut state = WatcherExclusionPaths::new(root.path(), &[]);
        EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = Some(mounted.clone()));
        EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        assert!(state.refresh_touched(&[mounted.join("deep")]));
        assert_eq!(state.paths(), vec![mounted.clone()]);
        assert!(!state.refresh_touched(&[mounted.clone()]));
        EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = None);
        assert!(state.refresh_touched(&[mounted]));
        assert!(state.paths().is_empty());
        EXCLUSION_TEST_OPENS.with(|opens| assert!(opens.borrow().is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn external_refresh_respects_native_exclusion_budget() {
        let root = exclusion_tempdir();
        let external = TempDir::new().unwrap();
        let entries: Vec<_> = (0..WATCHER_EXCLUSION_LIMIT)
            .map(|index| WatcherExclusion {
                path: root.path().join(format!("ignored-{index}")),
                source: WatcherExclusionSource::Gitignore,
            })
            .collect();
        let mut state = WatcherExclusionPaths::new(root.path(), &entries);
        let linked = root.path().join("foo");
        std::os::unix::fs::symlink(external.path(), &linked).unwrap();
        assert!(!state.refresh_touched(&[linked.clone()]));
        assert_eq!(state.paths(), watcher_exclusion_paths(&entries));
        assert!(!WatcherWalkBoundary::for_root(root.path())
            .unwrap()
            .allows_directory(&linked));
    }

    #[test]
    fn exclusion_planner_timed_out_roots_retry_same_generation() {
        let pool = Arc::new(ExclusionPlannerPool::default());
        let started = Arc::new(AtomicU64::new(0));
        let mut releases = Vec::new();
        let mut callers = Vec::new();
        for _ in 0..8 {
            let root = TempDir::new().unwrap();
            let path = fs::canonicalize(root.path()).unwrap();
            fs::write(path.join("Cargo.toml"), "[workspace]\n").unwrap();
            fs::create_dir_all(path.join("target/debug")).unwrap();
            let mut builder = GitignoreBuilder::new(&path);
            builder.add_line(None, "/target/").unwrap();
            let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
            let (release, blocked) = mpsc::channel::<()>();
            releases.push(release);
            let pool = pool.clone();
            let started = started.clone();
            callers.push(thread::spawn(move || {
                let counters = crate::context::WatcherCounters::default();
                let mut retry = WatcherExclusionRetry::default();
                let worker_path = path.clone();
                let worker_matcher = matcher.clone();
                let outcome = retry
                    .attempt(&path, 1, &counters, || {
                        run_bounded_exclusion_planner(Duration::from_millis(100), pool, move || {
                            started.fetch_add(1, Ordering::AcqRel);
                            let _ = blocked.recv();
                            derive_exclusion_plan(
                                &worker_path,
                                &worker_matcher,
                                Some(WATCHER_EXCLUSION_LIMIT),
                                crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
                            )
                        })
                    })
                    .expect("initial attempt is due");
                assert!(
                    outcome.is_err(),
                    "no root can finish while both workers are held"
                );
                (root, path, matcher, retry, counters, outcome.unwrap_err())
            }));
        }
        let roots = callers
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(started.load(Ordering::Acquire), 2);
        assert_eq!(*pool.in_flight.lock().unwrap(), 2);
        assert_eq!(
            roots
                .iter()
                .filter(|root| root.5 == ExclusionPlanUnavailable::IoDeadline)
                .count(),
            2
        );
        assert_eq!(
            roots
                .iter()
                .filter(|root| root.5 == ExclusionPlanUnavailable::CapacityDeadline)
                .count(),
            6
        );

        // The first two workers outlive their callers' deadlines. Release them
        // and wait until both have returned their permits: until then the
        // retries below would race those workers for capacity.
        drop(releases);
        {
            let hang_guard = Instant::now() + Duration::from_secs(60);
            let mut in_flight = pool.in_flight.lock().unwrap();
            while *in_flight > 0 {
                let remaining = hang_guard.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "released planner workers never returned their permits"
                );
                in_flight = pool.available.wait_timeout(in_flight, remaining).unwrap().0;
            }
        }

        // Drive WatcherExclusionRetry for each root in turn, as a native
        // backend poll loop does, without changing any root's matcher
        // generation. Each plan is computed before its attempt, so the
        // bounded worker only returns it and the attempt's 2 s budget does not
        // time the filesystem walk. That budget covers both the wait for a
        // permit and the plan: on a loaded Windows runner one plan of these
        // small roots took up to 1.2 s, and eight concurrent retries planning
        // inside it ran out of time while queued for the pool's two permits.
        // The attempt still takes a permit from the shared pool.
        for (root, path, matcher, mut retry, counters, _) in roots {
            let planned = derive_exclusion_plan(
                &path,
                &matcher,
                Some(WATCHER_EXCLUSION_LIMIT),
                crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
            );
            let plan = retry
                .attempt_at(
                    &path,
                    1,
                    &counters,
                    Instant::now() + Duration::from_secs(10),
                    || {
                        run_bounded_exclusion_planner(
                            Duration::from_secs(2),
                            pool.clone(),
                            move || planned,
                        )
                    },
                )
                .expect("an unavailable generation must not be consumed")
                .expect("capacity is free");
            assert!(
                watcher_exclusion_paths(&plan.selected).contains(&path.join("target")),
                "same-generation retry must install target/: {plan:?}"
            );
            let health = counters.snapshot().exclusion_planning.unwrap();
            assert_eq!(health.matcher_generation, 1);
            assert_eq!(health.attempts, 2);
            assert_eq!(health.status, "planned");
            drop(root);
        }
    }

    fn assert_unavailable_generation_replan(keep_target_ignored: bool) {
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        let modules = root.path().join("node_modules");
        for path in [&target, &modules] {
            fs::create_dir(path).unwrap();
        }
        let make_matcher = |rules: &[&str]| {
            let mut builder = GitignoreBuilder::new(root.path());
            for rule in rules {
                builder.add_line(None, rule).unwrap();
            }
            Some(Arc::new(builder.build().unwrap()))
        };
        let matcher: SharedGitignore = Arc::new(std::sync::RwLock::new(make_matcher(&[
            "target/",
            "node_modules/",
        ])));
        let initial = WatcherExclusionPlan {
            selected: [&target, &modules]
                .into_iter()
                .map(|path| WatcherExclusion {
                    path: path.clone(),
                    source: WatcherExclusionSource::Gitignore,
                })
                .collect(),
            dropped: Vec::new(),
        };
        let counters = crate::context::WatcherCounters::default();
        let mut retry = WatcherExclusionRetry::default();
        let initial = retry
            .attempt(root.path(), 1, &counters, || Ok(initial))
            .unwrap()
            .unwrap();
        let mut installed = WatcherExclusionPaths::new(root.path(), &initial.selected);
        let mut stream_paths = installed.paths();
        let mut watched = BTreeSet::from([root.path().to_path_buf()]);
        let mut stream_restarts = 0;
        *matcher.write().unwrap() = make_matcher(if keep_target_ignored {
            &["target/", "node_modules/"]
        } else {
            &["node_modules/"]
        });
        for attempt in 0..EXCLUSION_PLAN_MAX_ATTEMPTS {
            if retry
                .replacement_at(
                    root.path(),
                    2,
                    &counters,
                    &mut installed,
                    &matcher,
                    Instant::now() + Duration::from_secs((attempt as u64 + 1) * 10),
                    || Err(ExclusionPlanUnavailable::CapacityDeadline),
                )
                .is_some()
            {
                let paths = installed.paths();
                // Inotify watches the directories outside the exclusion set;
                // FSEvents restarts its stream only if that set changes.
                watched = [root.path().to_path_buf(), target.clone(), modules.clone()]
                    .into_iter()
                    .filter(|path| !paths.contains(path))
                    .collect();
                if stream_paths != paths {
                    stream_paths = paths;
                    stream_restarts += 1;
                }
            }
            assert_eq!(
                watched.contains(&target),
                !keep_target_ignored,
                "a removed ignore rule must restore the directory watch even without a plan"
            );
            assert!(
                !watched.contains(&modules),
                "still-ignored directories stay excluded"
            );
            let expected = if keep_target_ignored {
                vec![target.clone(), modules.clone()]
            } else {
                vec![modules.clone()]
            };
            assert_eq!(installed.paths(), expected);
            assert_eq!(stream_paths, expected);
            assert_eq!(stream_restarts, usize::from(!keep_target_ignored));
        }
        let health = counters.snapshot().exclusion_planning.unwrap();
        assert_eq!(health.matcher_generation, 2);
        assert_eq!(health.attempts, EXCLUSION_PLAN_MAX_ATTEMPTS);
        assert_eq!(health.status, "exhausted");
    }

    #[test]
    fn unavailable_generation_replan_reincludes_unignored_directory() {
        assert_unavailable_generation_replan(false);
    }

    #[test]
    fn unavailable_generation_replan_keeps_ignored_exclusions_without_stream_restart() {
        assert_unavailable_generation_replan(true);
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_generation_replan_preserves_non_gitignore_exclusions() {
        let root = exclusion_tempdir();
        let external = TempDir::new().unwrap();
        let source = root.path().join("source");
        let ecosystem = root.path().join("build-cache");
        let observed = root.path().join("burst-cache");
        let linked = root.path().join("external");
        for path in [&source, &ecosystem, &observed] {
            fs::create_dir(path).unwrap();
        }
        std::os::unix::fs::symlink(external.path(), &linked).unwrap();
        let mut builder = GitignoreBuilder::new(root.path());
        builder.add_line(None, "source/").unwrap();
        let matcher: SharedGitignore = Arc::new(std::sync::RwLock::new(Some(Arc::new(
            builder.build().unwrap(),
        ))));
        let initial = WatcherExclusionPlan {
            selected: [
                (&source, WatcherExclusionSource::Gitignore),
                (&ecosystem, WatcherExclusionSource::Ecosystem),
                (&observed, WatcherExclusionSource::Observed),
                (&linked, WatcherExclusionSource::External),
            ]
            .into_iter()
            .map(|(path, source)| WatcherExclusion {
                path: path.clone(),
                source,
            })
            .collect(),
            dropped: Vec::new(),
        };
        let counters = crate::context::WatcherCounters::default();
        let mut retry = WatcherExclusionRetry::default();
        let initial = retry
            .attempt(root.path(), 1, &counters, || Ok(initial))
            .unwrap()
            .unwrap();
        let mut installed = WatcherExclusionPaths::new(root.path(), &initial.selected);
        let mut stream_paths = installed.paths();
        let mut watched = BTreeSet::from([root.path().to_path_buf()]);
        let mut stream_restarts = 0;
        *matcher.write().unwrap() = Some(Arc::new(
            GitignoreBuilder::new(root.path()).build().unwrap(),
        ));
        EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        for attempt in 0..EXCLUSION_PLAN_MAX_ATTEMPTS {
            if retry
                .replacement_at(
                    root.path(),
                    2,
                    &counters,
                    &mut installed,
                    &matcher,
                    Instant::now() + Duration::from_secs((attempt as u64 + 1) * 10),
                    || Err(ExclusionPlanUnavailable::CapacityDeadline),
                )
                .is_some()
            {
                let paths = installed.paths();
                watched = [root.path(), &source, &ecosystem, &observed, &linked]
                    .into_iter()
                    .filter(|path| !paths.iter().any(|excluded| path.starts_with(excluded)))
                    .map(Path::to_path_buf)
                    .collect();
                if stream_paths != paths {
                    stream_paths = paths;
                    stream_restarts += 1;
                }
            }
            assert!(
                watched.contains(&source),
                "removed ignore rules must restore source watches"
            );
            assert!(
                !watched.contains(&linked),
                "an external symlink must remain excluded without an ignore rule"
            );
            assert!(
                !watched.contains(&ecosystem),
                "ecosystem exclusions must survive matcher changes"
            );
            assert!(
                !watched.contains(&observed),
                "observed-volume exclusions must survive matcher changes"
            );
            assert_eq!(
                installed.paths(),
                vec![ecosystem.clone(), observed.clone(), linked.clone()]
            );
            assert_eq!(stream_paths, installed.paths());
            assert_eq!(stream_restarts, 1);
        }
        EXCLUSION_TEST_OPENS.with(|opens| assert!(opens.borrow().is_empty()));
        let health = counters.snapshot().exclusion_planning.unwrap();
        assert_eq!(health.attempts, EXCLUSION_PLAN_MAX_ATTEMPTS);
        assert_eq!(health.status, "exhausted");
    }

    #[test]
    fn unavailable_replans_preserve_installed_exclusions_until_success() {
        struct FakeBackend {
            exclusions: WatcherExclusionPaths,
            watch_set: BTreeSet<PathBuf>,
            stream_paths: Vec<PathBuf>,
            watch_swaps: usize,
            stream_restarts: usize,
        }
        impl FakeBackend {
            fn apply(&mut self, update: WatcherExclusionUpdate) {
                if let WatcherExclusionUpdate::Planned(plan) = update {
                    assert_eq!(
                        self.exclusions.paths(),
                        watcher_exclusion_paths(&plan.selected)
                    );
                }
                let paths = self.exclusions.paths();
                self.watch_set = paths.iter().cloned().collect();
                self.watch_swaps += 1;
                if self.stream_paths != paths {
                    self.stream_paths = paths;
                    self.stream_restarts += 1;
                }
            }
        }
        let root = TempDir::new().unwrap();
        let mut builder = GitignoreBuilder::new(root.path());
        builder.add_line(None, "target/").unwrap();
        builder.add_line(None, "node_modules/").unwrap();
        let matcher = Arc::new(std::sync::RwLock::new(Some(Arc::new(
            builder.build().unwrap(),
        ))));
        let make_plan = |names: &[&str]| WatcherExclusionPlan {
            selected: names
                .iter()
                .map(|name| WatcherExclusion {
                    path: root.path().join(name),
                    source: WatcherExclusionSource::Gitignore,
                })
                .collect(),
            dropped: Vec::new(),
        };
        let counters = crate::context::WatcherCounters::default();
        let mut retry = WatcherExclusionRetry::default();
        let initial = retry
            .attempt(root.path(), 1, &counters, || {
                Ok(make_plan(&["target", "node_modules"]))
            })
            .unwrap()
            .unwrap();
        let initial_paths = watcher_exclusion_paths(&initial.selected);
        let mut backend = FakeBackend {
            exclusions: WatcherExclusionPaths::new(root.path(), &initial.selected),
            watch_set: initial_paths.iter().cloned().collect(),
            stream_paths: initial_paths.clone(),
            watch_swaps: 0,
            stream_restarts: 0,
        };
        for (index, reason) in [
            ExclusionPlanUnavailable::CapacityDeadline,
            ExclusionPlanUnavailable::IoDeadline,
            ExclusionPlanUnavailable::SpawnFailed,
            ExclusionPlanUnavailable::WorkerDisconnected,
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(plan) = retry.replacement_at(
                root.path(),
                2,
                &counters,
                &mut backend.exclusions,
                &matcher,
                Instant::now() + Duration::from_secs((index as u64 + 1) * 10),
                || Err(reason),
            ) {
                backend.apply(plan);
            }
            assert_eq!(
                backend.exclusions.paths(),
                initial_paths,
                "unavailable planning must preserve installed exclusions"
            );
            assert_eq!(backend.watch_set, initial_paths.iter().cloned().collect());
            assert_eq!(backend.stream_paths, initial_paths);
            assert_eq!(
                backend.watch_swaps, 0,
                "failed attempts must not swap watches"
            );
            assert_eq!(
                backend.stream_restarts, 0,
                "failed attempts must not restart streams"
            );
            let health = counters.snapshot().exclusion_planning.unwrap();
            assert_eq!(health.matcher_generation, 2);
            assert_eq!(health.attempts, index as u32 + 1);
            assert_eq!(health.status, "retrying");
            assert_eq!(health.last_failure, Some(reason.as_str()));
        }
        let replacement = make_plan(&["target", "node_modules", "dist"]);
        let replacement_paths = watcher_exclusion_paths(&replacement.selected);
        let plan = retry
            .replacement_at(
                root.path(),
                2,
                &counters,
                &mut backend.exclusions,
                &matcher,
                Instant::now() + Duration::from_secs(60),
                || Ok(replacement),
            )
            .expect("completed replacement");
        backend.apply(plan);
        assert_eq!(backend.exclusions.paths(), replacement_paths);
        assert_eq!(
            backend.watch_set,
            replacement_paths.iter().cloned().collect()
        );
        assert_eq!(backend.stream_paths, replacement_paths);
        assert_eq!(backend.watch_swaps, 1);
        assert_eq!(backend.stream_restarts, 1);
        assert!(retry
            .replacement_at(
                root.path(),
                2,
                &counters,
                &mut backend.exclusions,
                &matcher,
                Instant::now() + Duration::from_secs(120),
                || panic!("generation was consumed")
            )
            .is_none());
    }

    #[test]
    fn unavailable_exclusion_plans_back_off_exhaust_and_report_health() {
        let root = TempDir::new().unwrap();
        for reason in [
            ExclusionPlanUnavailable::CapacityDeadline,
            ExclusionPlanUnavailable::IoDeadline,
            ExclusionPlanUnavailable::SpawnFailed,
            ExclusionPlanUnavailable::WorkerDisconnected,
        ] {
            EXCLUSION_RETRY_LOGS.with(|logs| logs.borrow_mut().clear());
            let counters = crate::context::WatcherCounters::default();
            let mut retry = WatcherExclusionRetry::default();
            for attempt in 1..=EXCLUSION_PLAN_MAX_ATTEMPTS {
                let now = Instant::now() + Duration::from_secs(u64::from(attempt) * 10);
                assert_eq!(
                    retry.attempt_at(root.path(), 7, &counters, now, || Err(reason)),
                    Some(Err(reason))
                );
                let health = counters.snapshot().exclusion_planning.unwrap();
                assert_eq!(health.attempts, attempt);
                assert_eq!(health.last_failure, Some(reason.as_str()));
                assert_eq!(
                    health.status,
                    if attempt == EXCLUSION_PLAN_MAX_ATTEMPTS {
                        "exhausted"
                    } else {
                        "retrying"
                    }
                );
                let before_retry = retry.retry_at.unwrap() - Duration::from_nanos(1);
                assert!(retry
                    .attempt_at(root.path(), 7, &counters, before_retry, || panic!(
                        "events must not bypass backoff"
                    ))
                    .is_none());
            }
            assert!(retry
                .attempt_at(
                    root.path(),
                    7,
                    &counters,
                    Instant::now() + Duration::from_secs(3600),
                    || panic!("exhausted planning must stop")
                )
                .is_none());
            EXCLUSION_RETRY_LOGS.with(|logs| {
                let logs = logs.borrow();
                assert_eq!(logs.len(), 1, "retry warning must be once per generation");
                assert!(logs[0].contains("generation=7"));
            });
            let health = serde_json::to_value(counters.snapshot()).unwrap();
            assert_eq!(health["exclusion_planning"]["status"], "exhausted");
            assert_eq!(
                health["exclusion_planning"]["attempt_limit"],
                EXCLUSION_PLAN_MAX_ATTEMPTS
            );

            assert_eq!(
                retry.attempt(root.path(), 8, &counters, || Ok(
                    WatcherExclusionPlan::default()
                )),
                Some(Ok(WatcherExclusionPlan::default()))
            );
            assert!(retry
                .attempt(root.path(), 8, &counters, || panic!(
                    "a genuine empty plan is final"
                ))
                .is_none());
            let health = counters.snapshot().exclusion_planning.unwrap();
            assert_eq!(health.status, "planned");
            assert_eq!(health.attempts, 1);
            assert_eq!(health.last_failure, None);
        }
    }

    #[test]
    fn exclusion_planner_waits_for_capacity_before_publishing_generation_plan() {
        let root = TempDir::new().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        fs::write(root_path.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::create_dir_all(root_path.join("target/debug")).unwrap();
        let mut builder = GitignoreBuilder::new(&root_path);
        builder.add_line(None, "/target/").unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let in_flight = Arc::new(ExclusionPlannerPool::default());
        let mut releases = Vec::new();
        let mut callers = Vec::new();
        for _ in 0..2 {
            let (release, blocked) = mpsc::channel::<()>();
            releases.push(release);
            let (started_tx, started_rx) = mpsc::channel();
            let permits = in_flight.clone();
            callers.push(thread::spawn(move || {
                run_bounded_exclusion_planner(Duration::from_secs(2), permits, move || {
                    started_tx.send(()).unwrap();
                    let _ = blocked.recv();
                    WatcherExclusionPlan {
                        selected: Vec::new(),
                        dropped: Vec::new(),
                    }
                })
            }));
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }

        // Backends consume each matcher generation once. A busy pool must not
        // turn that generation's only derivation into an empty exclusion set.
        let (entered_tx, entered_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let target = root_path.join("target");
        let caller = thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let plan =
                run_bounded_exclusion_planner(Duration::from_secs(2), in_flight, move || {
                    derive_exclusion_plan(
                        &root_path,
                        &matcher,
                        Some(WATCHER_EXCLUSION_LIMIT),
                        crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
                    )
                });
            let _ = result_tx.send(plan);
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                result_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "busy planners must wait within the caller deadline, not publish an empty plan"
        );
        drop(releases);
        let plan = result_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("capacity freed before the deadline");
        assert!(
            watcher_exclusion_paths(&plan.selected).contains(&target),
            "the waiting generation must install target/: {plan:?}"
        );
        caller.join().unwrap();
        for caller in callers {
            caller
                .join()
                .unwrap()
                .expect("released planner must finish");
        }
    }

    #[test]
    fn exclusion_planner_deadline_and_worker_limit_survive_blocked_io() {
        let in_flight = Arc::new(ExclusionPlannerPool::default());
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (release, blocked) = mpsc::channel::<()>();
            releases.push(release);
            let (started_tx, started_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let permits = in_flight.clone();
            let caller = thread::spawn(move || {
                let plan =
                    run_bounded_exclusion_planner(Duration::from_millis(20), permits, move || {
                        started_tx.send(()).unwrap();
                        let _ = blocked.recv();
                        WatcherExclusionPlan {
                            selected: Vec::new(),
                            dropped: Vec::new(),
                        }
                    });
                let _ = result_tx.send(plan);
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let result = result_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("planner must return while directory I/O is still blocked");
            assert_eq!(result, Err(ExclusionPlanUnavailable::IoDeadline));
            caller.join().unwrap();
        }
        assert_eq!(*in_flight.in_flight.lock().unwrap(), 2);
        let (started_tx, started_rx) = mpsc::channel();
        let result =
            run_bounded_exclusion_planner(Duration::from_millis(20), in_flight, move || {
                let _ = started_tx.send(());
                WatcherExclusionPlan {
                    selected: Vec::new(),
                    dropped: Vec::new(),
                }
            });
        assert_eq!(result, Err(ExclusionPlanUnavailable::CapacityDeadline));
        assert!(
            started_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a third blocked planner must not be spawned"
        );
        drop(releases);
    }

    #[cfg(unix)]
    #[test]
    fn observed_prefix_below_symlink_is_not_probed() {
        let root = exclusion_tempdir();
        let external = TempDir::new().unwrap();
        fs::create_dir_all(external.path().join("packages/deep/target")).unwrap();
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(external.path(), &link).unwrap();
        let boundary = WatcherWalkBoundary::for_root(root.path()).unwrap();
        assert!(!boundary.allows_directory(&link.join("packages/deep")));
        assert_eq!(
            directory_size_signal(&boundary, &link.join("packages/deep/target")),
            0
        );
        let counters = crate::context::watcher_counters_for_root(root.path());
        counters.set_observed_exclusion_prefixes(vec![crate::context::WatcherOverflowPrefix {
            prefix: "linked/packages/deep".into(),
            count: 42,
        }]);
        EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        let plan = derive_exclusion_plan(
            root.path(),
            &shared_matcher(root.path()),
            None,
            WatcherExclusionCoverage::ExactPath,
        );
        counters.set_observed_exclusion_prefixes(Vec::new());
        EXCLUSION_TEST_OPENS
            .with(|opens| assert!(!opens.borrow().iter().any(|path| path.starts_with(&link))));
        assert!(plan.selected.iter().any(
            |entry| entry.path() == link && entry.source() == WatcherExclusionSource::External
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_target_is_excluded_without_opening_it() {
        let root = exclusion_tempdir();
        let external = TempDir::new().unwrap();
        fs::write(root.path().join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        let target = root.path().join("target");
        std::os::unix::fs::symlink(external.path(), &target).unwrap();
        EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        let plan = derive_exclusion_plan(
            root.path(),
            &shared_matcher(root.path()),
            None,
            WatcherExclusionCoverage::Subtree,
        );
        let opens = EXCLUSION_TEST_OPENS.with(|opens| opens.borrow().clone());
        assert!(
            !opens.iter().any(|path| path.starts_with(&target)),
            "symlink target must never be opened: {opens:?}"
        );
        assert!(plan.selected.iter().any(|entry| entry.path() == target));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_large_tree_is_never_measured() {
        let root = exclusion_tempdir();
        let external = TempDir::new().unwrap();
        for index in 0..1100 {
            fs::create_dir(external.path().join(format!("package-{index}"))).unwrap();
        }
        let target = root.path().join("target");
        std::os::unix::fs::symlink(external.path(), &target).unwrap();
        let boundary = WatcherWalkBoundary::for_root(root.path()).unwrap();
        assert_eq!(
            directory_size_signal(&boundary, &target),
            0,
            "external contents are not a checkout size signal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn different_device_directory_is_excluded_without_opening_it() {
        let root = exclusion_tempdir();
        let mounted = root.path().join("mounted");
        fs::create_dir_all(mounted.join("deep/source")).unwrap();
        EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = Some(mounted.clone()));
        EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        let plan = derive_exclusion_plan(
            root.path(),
            &shared_matcher(root.path()),
            None,
            WatcherExclusionCoverage::Subtree,
        );
        EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = None);
        let opens = EXCLUSION_TEST_OPENS.with(|opens| opens.borrow().clone());
        assert!(
            !opens.iter().any(|path| path.starts_with(&mounted)),
            "different filesystem must never be opened: {opens:?}"
        );
        assert!(plan.selected.iter().any(|entry| entry.path() == mounted));
    }

    #[test]
    fn nested_ignore_rewrite_preserves_gitignore_syntax() {
        let dir = Path::new("foo[1]/bar*");

        assert_eq!(rewrite_nested_ignore_line(dir, ""), None);
        assert_eq!(rewrite_nested_ignore_line(dir, "# comment"), None);
        assert_eq!(
            rewrite_nested_ignore_line(dir, r"\#literal"),
            Some(r"foo\[1]/bar\*/**/\#literal".to_string())
        );
        assert_eq!(
            rewrite_nested_ignore_line(dir, r"\!literal"),
            Some(r"foo\[1]/bar\*/**/\!literal".to_string())
        );
        assert_eq!(
            rewrite_nested_ignore_line(dir, "!keep.log"),
            Some(r"!foo\[1]/bar\*/**/keep.log".to_string())
        );
        assert_eq!(
            rewrite_nested_ignore_line(dir, "/build"),
            Some(r"foo\[1]/bar\*/build".to_string())
        );
        assert_eq!(
            rewrite_nested_ignore_line(dir, "build/"),
            Some(r"foo\[1]/bar\*/**/build/".to_string())
        );
        assert_eq!(
            rewrite_nested_ignore_line(dir, "generated/output"),
            Some(r"foo\[1]/bar\*/generated/output".to_string())
        );
        assert_eq!(
            rewrite_nested_ignore_line(dir, "**/cache"),
            Some(r"foo\[1]/bar\*/**/cache".to_string())
        );
    }

    #[test]
    fn perf_audit_watcher_resolves_once_per_batch() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            root.join(".git/refs/heads/main"),
            "0123456789012345678901234567890123456789\n",
        )
        .unwrap();
        let config = WatcherFilterConfig::new(root.clone(), Some(root.join(".git")));
        let matcher = shared_matcher(&root);
        let mut paths = (0..1000)
            .map(|i| root.join(format!("src/file_{i:04}.rs")))
            .collect::<BTreeSet<_>>();
        paths.insert(root.join(".git/HEAD"));
        paths.insert(root.join(".git/refs/heads/main"));
        crate::search_index::audit_work_reset();
        let filtered = filter_canonical_paths(&config, &matcher, paths);
        assert_eq!(filtered.changed.len(), 1002);
        assert!(!filtered.ignore_file_changed);
        let work = crate::search_index::audit_work();
        assert_eq!(work.watcher_global, 1, "{work:?}");
        assert_eq!(work.watcher_head, 1, "{work:?}");
    }

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

    fn run_git(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(root)
                .args(args)
                .status()
                .expect("run git")
                .success(),
            "git {args:?} failed in {}",
            root.display()
        );
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
        // `.git` keeps slot zero; the measured directory takes the next slot,
        // ahead of every ecosystem name that produced no recorded events.
        assert_eq!(exclusions[0].path(), canonical_root.join(".git"));
        assert_eq!(exclusions[1].path(), hot);
        assert_eq!(exclusions[1].source(), WatcherExclusionSource::Observed);
        assert!(exclusions[2..]
            .iter()
            .all(|exclusion| exclusion.source() != WatcherExclusionSource::Observed));
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
        // `.git` keeps slot zero; the measured directory takes the next slot,
        // ahead of every ecosystem name that produced no recorded events.
        assert_eq!(exclusions[0].path(), canonical_root.join(".git"));
        assert_eq!(exclusions[1].path(), hot);
        assert_eq!(exclusions[1].source(), WatcherExclusionSource::Observed);
        assert!(exclusions[2..]
            .iter()
            .all(|exclusion| exclusion.source() != WatcherExclusionSource::Observed));
    }

    #[test]
    fn sibling_worktree_inherits_repository_ranking_on_first_bind() {
        let container = TempDir::new().unwrap();
        let storage = TempDir::new().unwrap();
        let main = container.path().join("main");
        let sibling = container.path().join("sibling");
        std::fs::create_dir(&main).unwrap();
        run_git(&main, &["init"]);
        std::fs::write(main.join(".gitignore"), "packages/*/tmp/\n").unwrap();
        std::fs::write(main.join("tracked.txt"), "repository identity\n").unwrap();
        run_git(&main, &["add", "."]);
        run_git(
            &main,
            &[
                "-c",
                "user.name=AFT Test",
                "-c",
                "user.email=aft@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        run_git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "watcher-sibling",
                sibling.to_str().unwrap(),
            ],
        );

        let main = std::fs::canonicalize(main).unwrap();
        let sibling = std::fs::canonicalize(sibling).unwrap();
        let relative_hot = "packages/opencode-plugin/tmp";
        std::fs::create_dir_all(main.join(relative_hot)).unwrap();
        std::fs::create_dir_all(sibling.join(relative_hot)).unwrap();
        assert_ne!(
            crate::path_identity::project_scope_key(&main),
            crate::path_identity::project_scope_key(&sibling)
        );
        assert_eq!(
            crate::search_index::artifact_cache_key(&main),
            crate::search_index::artifact_cache_key(&sibling)
        );

        let db = Arc::new(Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        let main_counters = crate::context::watcher_counters_for_root(&main);
        main_counters.set_observed_exclusion_prefixes(vec![
            crate::context::WatcherOverflowPrefix {
                prefix: relative_hot.to_string(),
                count: 91,
            },
        ]);
        persist_watcher_observations(&main, &main_counters, Some(&db));

        let sibling_counters = crate::context::watcher_counters_for_root(&sibling);
        sibling_counters.set_observed_exclusion_prefixes(Vec::new());
        load_watcher_observations(&sibling, &sibling_counters, Some(&db));

        assert_eq!(
            sibling_counters.observed_exclusion_prefixes(),
            vec![crate::context::WatcherOverflowPrefix {
                prefix: relative_hot.to_string(),
                count: 91,
            }]
        );
        let matcher = shared_matcher(&sibling);
        let exclusions =
            derive_excluded_subtrees(&sibling, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        assert_eq!(exclusions[0].path(), sibling.join(relative_hot));
        assert_eq!(exclusions[0].source(), WatcherExclusionSource::Observed);
    }

    #[test]
    fn git_directory_is_slot_zero_but_git_file_is_not_seeded() {
        let checkout = TempDir::new().unwrap();
        std::fs::create_dir(checkout.path().join(".git")).unwrap();
        std::fs::write(checkout.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(checkout.path().join(".gitignore"), "target/\n").unwrap();
        let checkout_root = std::fs::canonicalize(checkout.path()).unwrap();
        let checkout_matcher = shared_matcher(&checkout_root);

        let checkout_exclusions = derive_excluded_subtrees(
            &checkout_root,
            &checkout_matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
        );

        assert_eq!(checkout_exclusions[0].path(), checkout_root.join(".git"));
        assert_eq!(
            checkout_exclusions[0].source(),
            WatcherExclusionSource::Ecosystem
        );
        assert_eq!(checkout_exclusions[1].path(), checkout_root.join("target"));

        let linked = TempDir::new().unwrap();
        std::fs::write(
            linked.path().join(".git"),
            "gitdir: ../main/.git/worktrees/linked\n",
        )
        .unwrap();
        std::fs::write(linked.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(linked.path().join(".gitignore"), "target/\n").unwrap();
        let linked_root = std::fs::canonicalize(linked.path()).unwrap();
        let linked_matcher = shared_matcher(&linked_root);

        let linked_exclusions =
            derive_excluded_subtrees(&linked_root, &linked_matcher, Some(WATCHER_EXCLUSION_LIMIT));

        assert_eq!(
            watcher_exclusion_paths(&linked_exclusions),
            vec![linked_root.join("target")]
        );
    }

    #[test]
    fn ecosystem_manifest_gates_seed_only_when_the_root_marker_exists() {
        let cases: [(&str, &[&str]); 10] = [
            ("Cargo.toml", &["target"]),
            ("package.json", &["node_modules"]),
            ("build.gradle", &[".gradle"]),
            ("build.gradle.kts", &[".gradle"]),
            ("settings.gradle", &[".gradle"]),
            ("settings.gradle.kts", &[".gradle"]),
            ("pubspec.yaml", &[".dart_tool"]),
            ("Podfile", &["Pods"]),
            ("App.xcodeproj", &["DerivedData"]),
            ("App.xcworkspace", &["DerivedData"]),
        ];

        for (marker, directories) in cases {
            let root = TempDir::new().unwrap();
            std::fs::write(
                root.path().join(".gitignore"),
                directories
                    .iter()
                    .map(|directory| format!("{directory}/\n"))
                    .collect::<String>(),
            )
            .unwrap();
            let canonical_root = std::fs::canonicalize(root.path()).unwrap();
            let matcher = shared_matcher(&canonical_root);

            let without_marker = derive_excluded_subtrees(&canonical_root, &matcher, None);
            assert!(
                directories.iter().all(|directory| without_marker
                    .iter()
                    .all(|exclusion| exclusion.path() != canonical_root.join(directory))),
                "{marker} gate seeded without its root marker"
            );

            let marker_path = root.path().join(marker);
            if marker.ends_with(".xcodeproj") || marker.ends_with(".xcworkspace") {
                std::fs::create_dir(marker_path).unwrap();
            } else {
                std::fs::write(marker_path, "fixture\n").unwrap();
            }
            let with_marker = derive_excluded_subtrees(&canonical_root, &matcher, None);
            for directory in directories {
                let exclusion = with_marker
                    .iter()
                    .find(|exclusion| exclusion.path() == canonical_root.join(directory))
                    .unwrap_or_else(|| panic!("{marker} did not seed {directory}"));
                assert_eq!(exclusion.source(), WatcherExclusionSource::Ecosystem);
            }
        }
    }

    #[test]
    fn ungated_ecosystem_exclusions_are_seeded_when_ignored() {
        let root = TempDir::new().unwrap();
        let directories = [
            "build", "dist", ".next", ".turbo", ".cache", "coverage", "out",
        ];
        std::fs::write(
            root.path().join(".gitignore"),
            directories
                .iter()
                .map(|directory| format!("{directory}/\n"))
                .collect::<String>(),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);

        let exclusions = derive_excluded_subtrees(&canonical_root, &matcher, None);

        assert_eq!(
            watcher_exclusion_paths(&exclusions),
            directories
                .iter()
                .map(|directory| canonical_root.join(directory))
                .collect::<Vec<_>>()
        );
        assert!(exclusions
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Ecosystem));
    }

    #[test]
    fn exclusion_derivation_uses_fixed_priority_caps_and_skips_missing_nonseeds() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(root.path().join("build.gradle"), "plugins {}\n").unwrap();
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname='fixture'\n",
        )
        .unwrap();
        let priorities = [
            "target",
            "node_modules",
            ".venv",
            "venv",
            "__pycache__",
            "build",
            "dist",
            ".next",
            ".turbo",
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
            watcher_exclusion_paths(&exclusions),
            priorities[..WATCHER_EXCLUSION_LIMIT]
                .iter()
                .map(|name| std::fs::canonicalize(root.path().join(name)).unwrap())
                .collect::<Vec<_>>()
        );
        assert!(exclusions
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Ecosystem));
        assert!(!exclusions
            .iter()
            .any(|exclusion| exclusion.path().ends_with("missing")));
        assert!(!exclusions
            .iter()
            .any(|exclusion| exclusion.path().ends_with("other-generated")));
    }

    #[test]
    fn exclusion_derivation_seeds_only_ignored_absent_priority_directories() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "target/\nnode_modules/\ngenerated/\ndist/\n!dist/\n",
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);

        let exclusions = derive_excluded_subtrees(&canonical_root, &matcher, None);

        assert_eq!(
            watcher_exclusion_paths(&exclusions),
            ["target", "node_modules"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert!(exclusions
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Ecosystem));

        // Ecosystem representatives stay ahead of the generic `generated`
        // boundary even when their directories do not yet exist. If a later
        // build creates `target` or `node_modules`, the reserved exclusion
        // covers its writes immediately.
        std::fs::create_dir(canonical_root.join("generated")).unwrap();
        let exclusions = derive_excluded_subtrees(&canonical_root, &matcher, None);

        assert_eq!(
            watcher_exclusion_paths(&exclusions),
            ["target", "node_modules", "generated"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert_eq!(exclusions[2].source(), WatcherExclusionSource::Gitignore);
    }

    /// A TypeScript worktree whose only heavy ignored directory is a nested
    /// `node_modules`, with reserved root names that will never exist
    /// competing for the same slots.
    #[test]
    fn nested_node_modules_outranks_absent_root_seeds_in_typescript_worktree() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        // `node_modules` is last so a gitignore-ordered candidate for the
        // nested copy would lose to the three boundaries above it: the nested
        // copy has to win its slot as an ecosystem name, not as a line number.
        std::fs::write(
            root.path().join(".gitignore"),
            "tmp-logs/\ngenerated/\nreports/\ndist\npackages/plugin/dist\npackages/pi-plugin/dist\nnode_modules\n",
        )
        .unwrap();
        for existing in [
            "tmp-logs",
            "generated",
            "reports",
            "packages/plugin/node_modules",
            "packages/plugin/dist",
            "packages/pi-plugin/dist",
        ] {
            std::fs::create_dir_all(root.path().join(existing)).unwrap();
        }
        // An install target holds a real dependency tree; the sibling `dist`
        // copies are what a build left behind.
        for index in 0..4 {
            std::fs::write(
                root.path()
                    .join("packages/plugin/dist")
                    .join(format!("chunk-{index}.js")),
                b"export {};\n",
            )
            .unwrap();
        }
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);
        let nested_node_modules = canonical_root.join("packages/plugin/node_modules");
        for absent in ["target", "build", "dist", ".cache", "coverage"] {
            assert!(!canonical_root.join(absent).exists());
        }

        let exclusions = derive_excluded_subtrees(&canonical_root, &matcher, Some(3));

        assert_eq!(
            watcher_exclusion_paths(&exclusions),
            [
                "packages/plugin/node_modules",
                "packages/plugin/dist",
                "packages/pi-plugin/dist",
            ]
            .iter()
            .map(|relative| canonical_root.join(relative))
            .collect::<Vec<_>>()
        );
        assert!(exclusions
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Ecosystem));
        assert!(
            exclusions
                .iter()
                .any(|exclusion| exclusion.path() == nested_node_modules),
            "the directory producing the events must hold a slot"
        );
        for absent_seed in ["node_modules", "dist"] {
            let absent_seed = canonical_root.join(absent_seed);
            assert!(
                !exclusions
                    .iter()
                    .any(|exclusion| exclusion.path() == absent_seed),
                "absent root seed {} displaced an existing ignored directory",
                absent_seed.display()
            );
        }
    }

    #[test]
    fn same_name_copies_in_distinct_subtrees_need_their_own_slots() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "node_modules\n").unwrap();
        for existing in [
            "node_modules",
            "packages/one/node_modules",
            "packages/two/node_modules",
        ] {
            std::fs::create_dir_all(root.path().join(existing)).unwrap();
        }
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);
        let nested = ["packages/one/node_modules", "packages/two/node_modules"]
            .iter()
            .map(|relative| canonical_root.join(relative))
            .collect::<Vec<_>>();

        let exact_path = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::ExactPath,
        );
        let subtree = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::Subtree,
        );

        // An exact-path kernel filter compares the path it was handed, so the
        // root copy covers neither nested copy and each needs its own entry.
        assert_eq!(
            watcher_exclusion_paths(&exact_path.selected),
            std::iter::once(canonical_root.join("node_modules"))
                .chain(nested.iter().cloned())
                .collect::<Vec<_>>()
        );
        // These are siblings, not descendants of the root node_modules. Even
        // a subtree backend must exclude each one independently.
        assert_eq!(
            watcher_exclusion_paths(&subtree.selected),
            watcher_exclusion_paths(&exact_path.selected)
        );
    }

    #[test]
    fn observed_prefix_deepens_to_the_ignored_child_that_flooded() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "node_modules\ndist\n").unwrap();
        for existing in [
            "packages/plugin/node_modules",
            "packages/plugin/src",
            "packages/plugin/dist",
        ] {
            std::fs::create_dir_all(root.path().join(existing)).unwrap();
        }
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);
        // What the ring recorded in the specimen: two components of the
        // flooding path, naming a tracked package directory that no exclusion
        // can ever cover.
        crate::context::watcher_counters_for_root(&canonical_root).set_observed_exclusion_prefixes(
            vec![crate::context::WatcherOverflowPrefix {
                prefix: "packages/plugin".to_string(),
                count: 42,
            }],
        );

        let exclusions =
            derive_excluded_subtrees(&canonical_root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));

        let credited = exclusions
            .iter()
            .filter(|exclusion| exclusion.source() == WatcherExclusionSource::Observed)
            .map(|exclusion| exclusion.path().to_path_buf())
            .collect::<Vec<_>>();
        assert_eq!(
            credited,
            ["packages/plugin/dist", "packages/plugin/node_modules"]
                .iter()
                .map(|relative| canonical_root.join(relative))
                .collect::<Vec<_>>()
        );
        assert!(
            !exclusions
                .iter()
                .any(|exclusion| exclusion.path() == canonical_root.join("packages/plugin")),
            "a tracked directory must never be excluded"
        );
        crate::context::watcher_counters_for_root(&canonical_root)
            .set_observed_exclusion_prefixes(Vec::new());
    }

    /// Paths reported by Windows can have both `\\?\` and backslash separators;
    /// neither changes which flooding subtree the path belongs to.
    fn is_flooding_ignored_path(path: &Path) -> bool {
        let rendered = path.to_string_lossy().replace('\\', "/");
        let normalized = rendered.strip_prefix("//?/").unwrap_or(&rendered);
        for child in ["prompts", "athena"] {
            let subtree = format!("/.cortexkit/alfonso/{child}");
            if normalized.ends_with(&subtree) || normalized.contains(&format!("{subtree}/")) {
                return true;
            }
        }
        normalized.rsplit_once('/').is_some_and(|(parent, file)| {
            parent.ends_with("/.cortexkit/alfonso")
                && file.starts_with("ledger-")
                && file.ends_with(".md")
        })
    }

    /// The shape of a repository that keeps one agent-tooling directory
    /// visible while ignoring what it holds: `.cortexkit/alfonso` itself is
    /// re-included, its children are ignored, and one child is re-included
    /// again because CI reads it. The parent can never be excluded, so after
    /// an overflow the volume has to land on the ignored children that wrote
    /// it, and those children have to outrank the `node_modules` copies that
    /// were already sitting in every slot while the flood went unwatched.
    #[test]
    fn overflow_under_a_reincluded_directory_excludes_its_flooding_ignored_child() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "node_modules\ntarget/\n.cortexkit/*\n!.cortexkit/alfonso/\n.cortexkit/alfonso/*\n!.cortexkit/alfonso/release-notes/\n",
        )
        .unwrap();
        // Enough existing ecosystem directories to fill every slot on their
        // own, as the node_modules copies of a JS monorepo do.
        for existing in [
            "target",
            "node_modules",
            "packages/plugin/node_modules",
            "packages/pi-plugin/node_modules",
            "packages/cli/node_modules",
            "packages/dashboard/node_modules",
            "packages/docs/node_modules",
            "packages/e2e-tests/node_modules",
            ".cortexkit/alfonso/prompts",
            ".cortexkit/alfonso/athena",
            ".cortexkit/alfonso/audits",
            ".cortexkit/alfonso/release-notes",
        ] {
            std::fs::create_dir_all(root.path().join(existing)).unwrap();
        }
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let alfonso = canonical_root.join(".cortexkit/alfonso");
        // Ignored files directly under the re-included directory: they are
        // ignored, but no directory exclusion can ever name them.
        for index in 0..40 {
            std::fs::write(alfonso.join(format!("ledger-{index}.md")), "").unwrap();
        }
        let matcher = shared_matcher(&canonical_root);
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let generation = Arc::new(AtomicU64::new(1));
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
        filter.delay_before_rescan_for_test = Duration::from_millis(2_500);
        let handle = thread::spawn(move || filter.run(raw_rx));

        let mut burst = Vec::new();
        burst.extend((0..200).map(|index| alfonso.join(format!("prompts/run-{index}.md"))));
        burst.extend((0..80).map(|index| alfonso.join(format!("athena/panel-{index}.json"))));
        burst.extend((0..40).map(|index| alfonso.join(format!("release-notes/v{index}.md"))));
        // Repeated writes to ignored top-level files. Recorded per file, each
        // would take an entry in the capped observation list that no
        // directory exclusion can ever use.
        for _ in 0..3 {
            burst.extend((0..40).map(|index| alfonso.join(format!("ledger-{index}.md"))));
        }
        for path in burst {
            raw_tx
                .send(Ok(
                    notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path)
                ))
                .unwrap();
        }
        raw_tx
            .send(Ok(
                notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)
            ))
            .unwrap();
        // The filter flushes a path batch when its window expires. Under load
        // that batch can legitimately arrive before it consumes the overflow
        // event, so wait for the rescan rather than assuming it is first.
        // Wait for the filter's rescan-required dispatch, not for the
        // downstream filesystem scan. Under load, Windows CI needs a generous
        // hang guard while the worker consumes and sends the control event.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for overflow rescan"
            );
            match dispatch_rx.recv_timeout(remaining) {
                Ok(
                    WatcherDispatchEvent::Paths(paths)
                    | WatcherDispatchEvent::ContentOnlyPaths(paths),
                ) => {
                    let ignored = paths
                        .iter()
                        .filter(|path| is_flooding_ignored_path(path))
                        .collect::<Vec<_>>();
                    assert!(
                        ignored.is_empty(),
                        "flooding ignored paths must not be dispatched before overflow rescan: {ignored:?}"
                    );
                }
                Ok(WatcherDispatchEvent::RescanRequired(reason)) => {
                    assert_eq!(reason, RescanReason::Unknown);
                    break;
                }
                Ok(event) => panic!("unexpected dispatch before overflow rescan: {event:?}"),
                Err(error) => panic!("timed out waiting for overflow rescan: {error}"),
            }
        }
        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();

        // Volume is recorded against the ignored children that can be
        // excluded, never against an ignored file or a re-included path.
        assert_eq!(
            counters.observed_exclusion_prefixes(),
            vec![
                crate::context::WatcherOverflowPrefix {
                    prefix: ".cortexkit/alfonso/prompts".to_string(),
                    count: 200,
                },
                crate::context::WatcherOverflowPrefix {
                    prefix: ".cortexkit/alfonso/athena".to_string(),
                    count: 80,
                },
            ]
        );

        // Pin exact-path coverage: it is the coverage under which every
        // node_modules copy needs its own slot and the slots run out.
        let selected = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::ExactPath,
        )
        .selected;
        let paths = watcher_exclusion_paths(&selected);
        assert_eq!(
            paths[..3],
            [
                canonical_root.join(".git"),
                alfonso.join("prompts"),
                alfonso.join("athena"),
            ],
            "the ignored children that flooded must follow .git, heaviest first: {paths:?}"
        );
        assert!(selected[1..3]
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Observed));
        for never in [
            alfonso.join("release-notes"),
            alfonso.clone(),
            canonical_root.join(".cortexkit"),
        ] {
            assert!(
                !paths.contains(&never),
                "{} holds un-ignored paths and must never be excluded: {paths:?}",
                never.display()
            );
        }
        counters.set_observed_exclusion_prefixes(Vec::new());
    }

    /// Feed `burst` through a filter thread followed by an overflow, so the
    /// thread records observed volume the way a live overflow does.
    fn record_overflow_burst(root: &Path, matcher: &SharedGitignore, burst: Vec<PathBuf>) {
        let burst_len = burst.len();
        assert!(burst_len > 0 && burst_len <= WATCHER_ATTRIBUTION_RING_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = crossbeam_channel::bounded(1);
        let (raw_tx, raw_rx) = mpsc::channel();
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.to_path_buf(), None),
            Arc::clone(matcher),
            Arc::new(AtomicU64::new(1)),
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let counters = Arc::clone(&filter.config.counters);
        // Queue the complete burst and overflow before intake can start.
        for path in burst {
            raw_tx
                .send(Ok(
                    notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path)
                ))
                .unwrap();
        }
        raw_tx
            .send(Ok(
                notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)
            ))
            .unwrap();
        let timeout = Duration::from_secs(30);
        let deadline = Instant::now() + timeout;
        // This fixture tests overflow attribution, not debounce timing. Keep
        // automatic flushing beyond the receive timeout even if canonicalizing
        // the queued paths takes longer than the normal 250 ms flush window.
        filter.flush_deadline = Some(deadline + timeout);
        let handle = thread::spawn(move || filter.run(raw_rx));
        let dispatched =
            dispatch_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        handle.join().unwrap();
        assert_eq!(
            dispatched.unwrap(),
            WatcherDispatchEvent::RescanRequired(RescanReason::Unknown)
        );
        // Diagnostic overflow prefixes count nonignored paths too, so their
        // total verifies the burst was observed even when the exclusion list
        // is empty.
        let prefixes = counters.snapshot().last_overflow_prefixes;
        assert_eq!(
            prefixes.iter().map(|prefix| prefix.count).sum::<u64>(),
            burst_len as u64,
            "overflow must record the entire burst: {prefixes:?}"
        );
    }

    const AGENT_SCRATCH_SIBLINGS: [&str; 7] = [
        "prompts", "drafts", "plans", "audit", "context", "handoffs", "notepads",
    ];

    /// A Rust + JS workspace with build output and dependency folders on disk,
    /// plus an agent scratch directory with seven busy sibling folders.
    fn mixed_workspace_with_agent_scratch(ignore_rules: &str) -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), ignore_rules).unwrap();
        for existing in [
            "target",
            "node_modules",
            "packages/opencode/node_modules",
            "packages/core/node_modules",
            "packages/core/dist",
            "modules/condition-runner/node_modules",
            ".cortexkit/alfonso/rules",
        ] {
            std::fs::create_dir_all(root.path().join(existing)).unwrap();
        }
        for sibling in AGENT_SCRATCH_SIBLINGS {
            std::fs::create_dir_all(root.path().join(".cortexkit/alfonso").join(sibling)).unwrap();
        }
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        (root, canonical_root)
    }

    fn agent_scratch_burst(root: &Path) -> Vec<PathBuf> {
        let alfonso = root.join(".cortexkit/alfonso");
        AGENT_SCRATCH_SIBLINGS
            .iter()
            .flat_map(|sibling| {
                let alfonso = alfonso.clone();
                (0..70).map(move |index| alfonso.join(sibling).join(format!("{index}.md")))
            })
            .collect()
    }

    /// Events spread over the sibling folders of an ignored directory collapse
    /// into one exclusion of that directory, and the build output and
    /// dependency folders on disk keep their slots.
    #[test]
    fn overflow_across_siblings_of_an_ignored_directory_costs_one_slot() {
        let (_root, canonical_root) = mixed_workspace_with_agent_scratch(
            ".cortexkit/alfonso/\nnode_modules/\ndist/\n/target/\n",
        );
        let matcher = shared_matcher(&canonical_root);
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        record_overflow_burst(
            &canonical_root,
            &matcher,
            agent_scratch_burst(&canonical_root),
        );

        assert_eq!(
            counters.observed_exclusion_prefixes(),
            vec![crate::context::WatcherOverflowPrefix {
                prefix: ".cortexkit/alfonso".to_string(),
                count: 490,
            }]
        );
        let alfonso = canonical_root.join(".cortexkit/alfonso");
        for coverage in [
            WatcherExclusionCoverage::ExactPath,
            WatcherExclusionCoverage::Subtree,
        ] {
            let selected = derive_exclusion_plan(
                &canonical_root,
                &matcher,
                Some(WATCHER_EXCLUSION_LIMIT),
                coverage,
            )
            .selected;
            let paths = watcher_exclusion_paths(&selected);
            assert_eq!(
                paths[0],
                canonical_root.join(".git"),
                "{coverage:?}: {paths:?}"
            );
            assert_eq!(paths[1], alfonso, "{coverage:?}: {paths:?}");
            // The `watcher exclusions:` log line labels this exclusion `by=observed`.
            assert_eq!(selected[1].source().as_str(), "observed");
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| path.starts_with(canonical_root.join(".cortexkit")))
                    .count(),
                1,
                "{coverage:?}: the ignored directory must hold exactly one slot: {paths:?}"
            );
            for kept in ["target", "node_modules"] {
                assert!(
                    paths.contains(&canonical_root.join(kept)),
                    "{coverage:?}: {kept} lost its slot: {paths:?}"
                );
            }
        }
        counters.set_observed_exclusion_prefixes(Vec::new());
    }

    /// The same flood where the scratch directory itself is not ignored: only
    /// its children are, and one child is re-included because it is tracked.
    /// The parent can never be excluded, so the siblings cannot collapse into
    /// one slot, and they must not take every slot either: `target`,
    /// `node_modules` and `dist` exist on disk and the next build or install
    /// would overflow the watcher again if they lost their exclusion.
    #[test]
    fn busy_sibling_folders_do_not_evict_existing_build_output_exclusions() {
        let (_root, canonical_root) = mixed_workspace_with_agent_scratch(
            ".cortexkit/alfonso/*\n!.cortexkit/alfonso/rules/\nnode_modules/\ndist/\n/target/\n",
        );
        let matcher = shared_matcher(&canonical_root);
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let alfonso = canonical_root.join(".cortexkit/alfonso");
        let mut burst = agent_scratch_burst(&canonical_root);
        burst.extend((0..20).map(|index| alfonso.join(format!("rules/{index}.md"))));
        record_overflow_burst(&canonical_root, &matcher, burst);

        for coverage in [
            WatcherExclusionCoverage::ExactPath,
            WatcherExclusionCoverage::Subtree,
        ] {
            let plan = derive_exclusion_plan(
                &canonical_root,
                &matcher,
                Some(WATCHER_EXCLUSION_LIMIT),
                coverage,
            );
            let paths = watcher_exclusion_paths(&plan.selected);
            assert_eq!(
                paths.len(),
                WATCHER_EXCLUSION_LIMIT,
                "{coverage:?}: {paths:?}"
            );
            assert_eq!(
                paths[0],
                canonical_root.join(".git"),
                "{coverage:?}: {paths:?}"
            );
            assert!(
                paths.contains(&canonical_root.join("target")),
                "{coverage:?}: target lost its slot: {paths:?}"
            );
            // Which copy represents a name is the representative rule's
            // choice; what matters here is that the name keeps one.
            for kept in ["node_modules", "dist"] {
                assert!(
                    paths
                        .iter()
                        .any(|path| path.file_name() == Some(std::ffi::OsStr::new(kept))),
                    "{coverage:?}: every {kept} lost its slot: {paths:?}"
                );
            }
            let observed = plan
                .selected
                .iter()
                .filter(|exclusion| exclusion.source() == WatcherExclusionSource::Observed)
                .count();
            assert!(
                observed >= 1,
                "{coverage:?}: the folders that flooded lost every slot: {paths:?}"
            );
            for never in [alfonso.clone(), alfonso.join("rules")] {
                assert!(
                    !paths.contains(&never),
                    "{coverage:?}: {} is not ignored and must never be excluded: {paths:?}",
                    never.display()
                );
            }
        }
        counters.set_observed_exclusion_prefixes(Vec::new());
    }

    /// A folder that is not ignored is never excluded, however busy: neither
    /// when its volume comes from a live overflow nor from a stored ranking.
    #[test]
    fn busy_folder_that_is_not_ignored_is_never_excluded() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "generated-cache/\n").unwrap();
        std::fs::create_dir_all(root.path().join("src/generated/nested")).unwrap();
        std::fs::create_dir_all(root.path().join("generated-cache")).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let busy = canonical_root.join("src/generated");
        record_overflow_burst(
            &canonical_root,
            &matcher,
            (0..300)
                .map(|index| busy.join(format!("nested/{index}.rs")))
                .collect(),
        );
        assert!(
            counters.observed_exclusion_prefixes().is_empty(),
            "volume in tracked folders has no excludable owner: {:?}",
            counters.observed_exclusion_prefixes()
        );

        // A ranking stored by an older build could still name the folder.
        counters.set_observed_exclusion_prefixes(vec![crate::context::WatcherOverflowPrefix {
            prefix: "src/generated".to_string(),
            count: 300,
        }]);
        for coverage in [
            WatcherExclusionCoverage::ExactPath,
            WatcherExclusionCoverage::Subtree,
        ] {
            let paths = watcher_exclusion_paths(
                &derive_exclusion_plan(
                    &canonical_root,
                    &matcher,
                    Some(WATCHER_EXCLUSION_LIMIT),
                    coverage,
                )
                .selected,
            );
            assert!(
                paths
                    .iter()
                    .all(|path| !path.starts_with(canonical_root.join("src"))),
                "{coverage:?}: a tracked folder was excluded: {paths:?}"
            );
        }
        counters.set_observed_exclusion_prefixes(Vec::new());
    }

    #[test]
    fn exclusion_plan_names_the_candidates_the_cap_dropped() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "node_modules\nfirst/\nsecond/\nthird/\nfourth/\n",
        )
        .unwrap();
        for existing in ["node_modules", "first", "second", "third", "fourth"] {
            std::fs::create_dir(root.path().join(existing)).unwrap();
        }
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);

        let plan = derive_watcher_exclusion_plan(&canonical_root, &matcher, Some(2)).unwrap();

        assert_eq!(
            watcher_exclusion_paths(&plan.selected),
            ["node_modules", "first"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            watcher_exclusion_paths(&plan.dropped),
            ["second", "third", "fourth"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn ecosystem_exclusions_rank_above_existing_gitignore_boundaries() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::create_dir(root.path().join("node_modules")).unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "generated/\nnode_modules/\ntarget/\n",
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);

        let exclusions = derive_excluded_subtrees(&canonical_root, &matcher, Some(3));

        // Each enabled ecosystem gets a representative before a second slot
        // goes to a generic ignored boundary. The absent target seed is safe
        // and begins covering that path if the first Rust build creates it.
        assert_eq!(
            watcher_exclusion_paths(&exclusions),
            ["target", "node_modules", "generated"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert_eq!(exclusions[0].source(), WatcherExclusionSource::Ecosystem);
        assert_eq!(exclusions[1].source(), WatcherExclusionSource::Ecosystem);
        assert_eq!(exclusions[2].source(), WatcherExclusionSource::Gitignore);

        std::fs::create_dir(canonical_root.join("target")).unwrap();
        let exclusions = derive_excluded_subtrees(&canonical_root, &matcher, Some(3));

        assert_eq!(
            watcher_exclusion_paths(&exclusions),
            ["target", "node_modules", "generated"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert_eq!(exclusions[2].source(), WatcherExclusionSource::Gitignore);
    }

    /// A subtree backend must not hand an ecosystem's reserved slot to an
    /// absent root copy when an existing nested copy is taking the writes.
    /// Root `dist` is not an ancestor of `packages/plugin/dist`, so excluding
    /// it covers nothing. This is pinned explicitly because the platform that
    /// runs subtree coverage is not the platform most of us develop on: the
    /// defect it guards reached CI green on macOS and failed only on Linux.
    #[test]
    fn subtree_representative_prefers_an_existing_copy_over_an_absent_root() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "dist\npackages/plugin/dist\nnode_modules\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("packages/plugin/dist")).unwrap();
        std::fs::create_dir_all(root.path().join("packages/plugin/node_modules")).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);
        assert!(!canonical_root.join("dist").exists());

        let selected = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::Subtree,
        )
        .selected;
        let paths = watcher_exclusion_paths(&selected);

        assert!(
            paths.contains(&canonical_root.join("packages/plugin/dist")),
            "the existing nested copy lost its slot to an absent root: {paths:?}"
        );
        assert!(
            !paths
                .first()
                .is_some_and(|first| first == &canonical_root.join("dist")),
            "an absent root sibling took the first slot and covers nothing: {paths:?}"
        );
    }

    #[test]
    fn exact_path_seed_keeps_rust_and_js_covered_when_node_modules_fill_every_slot() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        let node_modules = [
            "packages/plugin/node_modules",
            "packages/pi-plugin/node_modules",
            "packages/cli/node_modules",
            "packages/dashboard/node_modules",
            "packages/e2e-tests/node_modules",
            "packages/docs/node_modules",
            "node_modules",
            "packages/retina-local-fs/node_modules",
        ];
        for relative in node_modules {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        std::fs::write(root.path().join(".gitignore"), "node_modules\ntarget/\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);
        let target = canonical_root.join("target");

        let exclusions = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::ExactPath,
        )
        .selected;

        assert!(!target.exists(), "the fresh target seed must remain absent");
        assert!(
            exclusions
                .iter()
                .any(|exclusion| exclusion.path() == target),
            "the Rust ecosystem lost every slot: {:?}",
            watcher_exclusion_paths(&exclusions)
        );
        assert!(
            exclusions.iter().any(|exclusion| {
                exclusion.path().file_name() == Some(std::ffi::OsStr::new("node_modules"))
            }),
            "the JavaScript ecosystem lost every slot: {:?}",
            watcher_exclusion_paths(&exclusions)
        );
    }

    #[test]
    fn fresh_mixed_rust_node_root_reserves_ecosystem_exclusions_first() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::create_dir(root.path().join("node_modules")).unwrap();
        let packages = ["one", "two", "three", "four"];
        for package in packages {
            std::fs::create_dir_all(root.path().join(format!("packages/{package}/node_modules")))
                .unwrap();
        }
        std::fs::create_dir(root.path().join("generated")).unwrap();
        let nested_ignores = packages
            .iter()
            .map(|package| format!("/packages/{package}/node_modules/\n"))
            .collect::<String>();
        std::fs::write(
            root.path().join(".gitignore"),
            format!("/node_modules/\n{nested_ignores}/target/\n/generated/\n"),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = shared_matcher(&canonical_root);

        // Pin the exact-path coverage: this test is about copies that an
        // exact-path kernel filter must each name, and the compiled-in default
        // is Subtree on Linux, where the root copy covers the nested ones.
        let exclusions = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::ExactPath,
        )
        .selected;

        // The two root ecosystem names still take the first slots.
        assert_eq!(
            watcher_exclusion_paths(&exclusions[..2]),
            ["target", "node_modules"]
                .iter()
                .map(|name| canonical_root.join(name))
                .collect::<Vec<_>>()
        );
        assert!(exclusions[..2]
            .iter()
            .all(|exclusion| exclusion.source() == WatcherExclusionSource::Ecosystem));
        // Each workspace copy is a separate path to an exact-path kernel
        // filter, so each is seeded in its own right rather than deferred
        // behind the root copy that does not cover it.
        for package in packages {
            let nested = canonical_root.join(format!("packages/{package}/node_modules"));
            let seeded = exclusions
                .iter()
                .find(|exclusion| exclusion.path() == nested)
                .unwrap_or_else(|| panic!("{} was not seeded", nested.display()));
            assert_eq!(seeded.source(), WatcherExclusionSource::Ecosystem);
        }
        // A plain ignored boundary ranks behind every ecosystem name.
        let generated = exclusions.last().expect("a boundary is seeded last");
        assert_eq!(generated.path(), canonical_root.join("generated"));
        assert_eq!(generated.source(), WatcherExclusionSource::Gitignore);

        let observed_nested = PathBuf::from("packages/four/node_modules");
        crate::context::watcher_counters_for_root(&canonical_root).set_observed_exclusion_prefixes(
            vec![crate::context::WatcherOverflowPrefix {
                prefix: observed_nested.to_string_lossy().into_owned(),
                count: 200,
            }],
        );
        // Pin the exact-path coverage: this test is about copies that an
        // exact-path kernel filter must each name, and the compiled-in default
        // is Subtree on Linux, where the root copy covers the nested ones.
        let exclusions = derive_exclusion_plan(
            &canonical_root,
            &matcher,
            Some(WATCHER_EXCLUSION_LIMIT),
            WatcherExclusionCoverage::ExactPath,
        )
        .selected;
        let observed_position = exclusions
            .iter()
            .position(|exclusion| exclusion.path() == canonical_root.join(&observed_nested))
            .expect("the observed copy keeps its slot");
        let generated_position = exclusions
            .iter()
            .position(|exclusion| exclusion.path() == canonical_root.join("generated"))
            .expect("the ignored boundary keeps its slot");
        assert_eq!(
            exclusions[observed_position].source(),
            WatcherExclusionSource::Observed
        );
        assert!(
            observed_position < generated_position,
            "a directory the ring observed must outrank a plain ignored boundary: {:?}",
            watcher_exclusion_paths(&exclusions)
        );
        crate::context::watcher_counters_for_root(&canonical_root)
            .set_observed_exclusion_prefixes(Vec::new());
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
    fn sqlite_named_source_directory_events_reach_checkout_from_unrelated_cwd() {
        const CHILD: &str = "AFT_TEST_SQLITE_DIRECTORY_CWD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // A subprocess changes the daemon cwd without racing other unit tests.
            let cwd = TempDir::new().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "watcher_filter::tests::sqlite_named_source_directory_events_reach_checkout_from_unrelated_cwd", "--nocapture"])
                .env(CHILD, "1")
                .current_dir(cwd.path())
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed;"));
            return;
        }
        let project = TempDir::new().unwrap();
        let root = project.path().canonicalize().unwrap();
        assert_ne!(std::env::current_dir().unwrap(), root);
        let old = root.join("models.db");
        let new = root.join("renamed.sqlite");
        let (tx, rx) = watcher_dispatch_channel();
        let mut filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.clone(), None),
            shared_matcher(&root),
            Arc::new(AtomicU64::new(0)),
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let mut dispatch = |kind, paths: Vec<PathBuf>| {
            assert!(watcher_event_invalidates(&kind));
            assert!(filter.push_raw_paths(paths.clone()));
            assert!(filter.flush_pending());
            assert_eq!(
                rx.try_recv().unwrap_or_else(|error| panic!(
                    "{kind:?} directory event must reach checkout: {error}"
                )),
                WatcherDispatchEvent::Paths(paths)
            );
            assert!(rx.try_recv().is_err());
        };
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("source.rs"), "fn source() {}\n").unwrap();
        dispatch(EventKind::Create(CreateKind::Folder), vec![old.clone()]);
        std::fs::rename(&old, &new).unwrap();
        dispatch(
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
            vec![old, new.clone()],
        );
        std::fs::remove_dir_all(&new).unwrap();
        dispatch(
            EventKind::Remove(notify::event::RemoveKind::Folder),
            vec![new],
        );
        let file = root.join("fixture.db");
        std::fs::write(&file, "not source").unwrap();
        assert!(watcher_path_is_infra_skip_under(&root, &file));
    }

    #[test]
    fn atomic_save_paths_are_reported_unflushed_until_dispatched() {
        let project = TempDir::new().unwrap();
        let root = project.path().canonicalize().unwrap();
        let source = root.join("probe.rs");
        let temporary = root.join("probe.rs.save");
        std::fs::write(&temporary, "pub fn probe() {}\n").unwrap();
        std::fs::rename(&temporary, &source).unwrap();
        let (tx, rx) = watcher_dispatch_channel();
        let config = WatcherFilterConfig::new(root.clone(), None);
        let counters = Arc::clone(&config.counters);
        let mut filter = WatcherFilterThread::new(
            config,
            shared_matcher(&root),
            Arc::new(AtomicU64::new(0)),
            tx,
            Arc::new(AtomicBool::new(false)),
        );

        // A rename-over names the vanished temporary file and the replaced
        // original. Only the source file can change a call-graph answer.
        assert!(filter.push_raw_paths(vec![temporary.clone(), source.clone()]));
        assert_eq!(
            counters.unflushed_paths(),
            BTreeSet::from([source.clone()]),
            "a held source file must be visible before the flush window ends"
        );
        assert!(rx.try_recv().is_err(), "nothing is dispatched yet");

        assert!(filter.flush_pending());
        match rx.try_recv().expect("the flush dispatches the save") {
            WatcherDispatchEvent::Paths(paths) => assert!(paths.contains(&source), "{paths:?}"),
            other => panic!("unexpected dispatch: {other:?}"),
        }
        assert!(
            counters.unflushed_paths().is_empty(),
            "dispatched paths are the drain's to report"
        );
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
    fn os_metadata_write_schedules_no_refresh() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ds_store = root.join("nested/.DS_Store");
        let source = root.join("main.rs");
        std::fs::create_dir_all(ds_store.parent().unwrap()).unwrap();
        std::fs::write(&ds_store, [0u8; 16]).unwrap();
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let config = WatcherFilterConfig::new(root.clone(), None);
        let matcher = shared_matcher(&root);

        // The shared path filter drops it even without the thread's intake check.
        let filtered = filter_watcher_raw_paths_for_test(
            &config,
            &matcher,
            [ds_store.clone(), source.clone()],
        );
        assert_eq!(filtered.changed, BTreeSet::from([source.clone()]));
        assert!(!filtered.ignore_file_changed);

        let generation = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
        let (raw_tx, raw_rx) = mpsc::channel();
        let mut filter = WatcherFilterThread::new(
            config,
            matcher,
            generation,
            dispatch_tx,
            Arc::clone(&shutdown),
        );
        let handle = thread::spawn(move || filter.run(raw_rx));

        let mut write = notify::Event::new(EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Content,
        )));
        write.paths.push(ds_store);
        raw_tx.send(Ok(write)).unwrap();
        assert!(
            dispatch_rx
                .recv_timeout(WATCHER_FLUSH_WINDOW + Duration::from_millis(250))
                .is_err(),
            "a .DS_Store write must not dispatch a refresh"
        );

        // The same thread still dispatches a real source change, so the silence
        // above is the metadata skip rather than a stalled filter.
        let mut write = notify::Event::new(EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Content,
        )));
        write.paths.push(source.clone());
        raw_tx.send(Ok(write)).unwrap();
        assert_eq!(
            dispatch_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("source change dispatch"),
            WatcherDispatchEvent::ContentOnlyPaths(vec![source])
        );

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
            (0..3)
                .map(|index| root.join(format!("dropped-{index}")))
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
        assert!(
            line.contains("candidates_dropped=[dropped-0,dropped-1,dropped-2]"),
            "line: {line}"
        );
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
    fn ignore_file_inside_an_ignored_directory_is_not_a_rule_change() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join(".gitignore"), "/worker/.wrangler/tmp\n").unwrap();
        let dev_dir = root.join("worker/.wrangler/tmp/dev-1");
        std::fs::create_dir_all(&dev_dir).unwrap();
        std::fs::write(dev_dir.join(".gitignore"), "*\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/.gitignore"), "gen/\n").unwrap();
        let matcher = shared_matcher(&root);
        let config = WatcherFilterConfig::new(root.clone(), None);

        // wrangler writes a `.gitignore` into every dev directory it creates
        // under the already-ignored tmp tree: no rule change, no rebuild.
        let inside_ignored =
            filter_watcher_raw_paths_for_test(&config, &matcher, [dev_dir.join(".gitignore")]);
        assert!(!inside_ignored.ignore_file_changed);
        assert!(inside_ignored.ignore_file_paths.is_empty());
        assert!(inside_ignored.changed.is_empty());

        // A rule file in a visible directory, and the root rule file itself,
        // still count.
        let visible_path = root.join("src/.gitignore");
        let visible = filter_watcher_raw_paths_for_test(
            &config,
            &matcher,
            [visible_path.clone(), root.join("src/.aftignore")],
        );
        assert!(visible.ignore_file_changed);
        assert_eq!(
            visible.ignore_file_paths,
            BTreeSet::from([visible_path, root.join("src/.aftignore")])
        );
        let at_root =
            filter_watcher_raw_paths_for_test(&config, &matcher, [root.join(".gitignore")]);
        assert!(at_root.ignore_file_changed);
        assert_eq!(
            at_root.ignore_file_paths,
            BTreeSet::from([root.join(".gitignore")])
        );

        // With no matcher yet there is nothing to say the file or its parent is
        // ignored, so the conservative answer stands.
        let no_matcher: SharedGitignore = Arc::new(RwLock::new(None));
        let unknown_path = dev_dir.join(".gitignore");
        let unknown =
            filter_watcher_raw_paths_for_test(&config, &no_matcher, [unknown_path.clone()]);
        assert!(unknown.ignore_file_changed);
        assert_eq!(unknown.ignore_file_paths, BTreeSet::from([unknown_path]));
    }

    #[test]
    fn self_ignored_nested_gitignore_rewrites_still_report_rule_file_events() {
        const REWRITES: usize = 8;

        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let build_dir = root.join("engram-worker/build");
        std::fs::create_dir_all(&build_dir).unwrap();
        let ignore_path = build_dir.join(".gitignore");
        std::fs::write(&ignore_path, b"*").unwrap();

        let mut builder = GitignoreBuilder::new(&root);
        let rewritten = rewrite_nested_ignore_line(Path::new("engram-worker/build"), "*")
            .expect("bare wildcard is an effective nested rule");
        builder
            .add_line(Some(ignore_path.clone()), &rewritten)
            .unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let config = WatcherFilterConfig::new(root, None);

        for _ in 0..REWRITES {
            std::fs::write(&ignore_path, b"*").unwrap();
            let filtered =
                filter_watcher_raw_paths_for_test(&config, &matcher, [ignore_path.clone()]);
            assert!(filtered.ignore_file_changed);
            assert_eq!(
                filtered.ignore_file_paths,
                BTreeSet::from([ignore_path.clone()])
            );
            assert!(filtered.changed.is_empty());
        }
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

        let filtered = filter_watcher_raw_paths_for_test(&config, &matcher, [exclude.clone()]);

        assert!(filtered.ignore_file_changed);
        assert_eq!(filtered.ignore_file_paths, BTreeSet::from([exclude]));
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
