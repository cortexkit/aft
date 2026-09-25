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
const WATCHER_ATTRIBUTION_RING_CAPACITY: usize = 512;
const WATCHER_OVERFLOW_PREFIX_LIMIT: usize = 5;
const WATCHER_OBSERVED_EXCLUSION_LIMIT: usize = 32;

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
    IgnoreRulesChanged { paths: Vec<PathBuf> },
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

pub(crate) fn queued_path_is_ignored_by_matcher(matcher: &SharedGitignore, path: &Path) -> bool {
    // The raw filter deliberately admits HEAD/ref metadata as control events.
    // Retirement must not turn those already-admitted events into infra skips.
    if path.components().any(|part| part.as_os_str() == ".git") {
        return false;
    }
    let guard = matcher
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    watcher_path_is_ignored(guard.as_deref(), path)
}

fn watcher_path_is_ignored(matcher: Option<&Gitignore>, path: &Path) -> bool {
    matcher.is_some_and(|matcher| {
        let joined;
        let path = if path.is_relative() {
            joined = matcher.path().join(path);
            joined.as_path()
        } else {
            path
        };
        let canonical = std::fs::canonicalize(path).ok();
        let path = canonical.as_deref().unwrap_or(path);
        path.starts_with(matcher.path())
            && matcher
                .matched_path_or_any_parents(path, path.is_dir())
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
    matcher.is_some_and(|matcher| {
        let canonical = std::fs::canonicalize(path).ok();
        let path = canonical.as_deref().unwrap_or(path);
        path.starts_with(matcher.path())
            && matcher.matched_path_or_any_parents(path, true).is_ignore()
    })
}

/// True when `path` is an in-tree ignore rule file that the current matcher
/// already excludes, either directly or through an ignored parent directory.
/// Only in-tree rule files can satisfy this; the global excludes file and
/// `.git/info/exclude` never do.
fn ignore_file_is_ignored_by_matcher(matcher: &SharedGitignore, path: &Path) -> bool {
    if !watcher_path_is_ignore_file(path) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let guard = matcher
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_deref().is_some_and(|matcher| {
        path.starts_with(matcher.path())
            && (matcher.matched_path_or_any_parents(path, false).is_ignore()
                || (parent != matcher.path()
                    && matcher
                        .matched_path_or_any_parents(parent, true)
                        .is_ignore()))
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
            Self::Ecosystem => 0,
            Self::Observed => 1,
            Self::Gitignore => 2,
        }
    }

    /// How directly this source knows the directory is heavy, best first. Used
    /// only when two producers describe the same path: a recorded event count
    /// is a measurement, an ecosystem name is a well-founded guess, and an
    /// ignore rule only says the directory is not tracked.
    fn evidence_rank(self) -> u8 {
        match self {
            Self::Observed => 0,
            Self::Ecosystem => 1,
            Self::Gitignore => 2,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
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
    if root.join("pyproject.toml").is_file() || root.join("requirements.txt").is_file() {
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
        ".git" => root.join(".git").is_dir(),
        "target" => root.join("Cargo.toml").is_file(),
        "node_modules" => root.join("package.json").is_file(),
        ".venv" | "venv" | "__pycache__" => root_has_python_manifest(root),
        ".gradle" => root_has_gradle_manifest(root),
        ".dart_tool" => root.join("pubspec.yaml").is_file(),
        "Pods" => root.join("Podfile").is_file(),
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
fn exclusion_path_is_directory_or_absent(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// The exclusion set an OS watcher should install, plus what the cap cost.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatcherExclusionPlan {
    /// Exclusions to install, best first.
    pub(crate) selected: Vec<WatcherExclusion>,
    /// The next candidates that lost a slot to the cap, best first. Named in
    /// the overflow log so a flood specimen shows what the cap left watched.
    pub(crate) dropped: Vec<WatcherExclusion>,
}

/// Cheap size signal for one directory: how many entries its top level holds,
/// counted up to `WATCHER_DIRECTORY_SIZE_SAMPLE_CAP`.
///
/// Nothing recursive happens here. The number only has to separate a populated
/// copy of a name from an empty one when both compete for the same slot, and a
/// capped `read_dir` is the one way to ask that on every platform (`st_blocks`
/// is Unix-only).
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn directory_size_signal(path: &Path) -> usize {
    match fs::read_dir(path) {
        Ok(entries) => entries.take(WATCHER_DIRECTORY_SIZE_SAMPLE_CAP).count(),
        Err(_) => 0,
    }
}

/// Is this a directory the exclusion walk must never enter or seed by itself?
///
/// A repository's own `.git` is seeded deliberately as slot zero; every other
/// `.git` (submodule, nested checkout) is a large tree with nothing indexable
/// in it, so the walk stops at the name rather than reading `objects/`.
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
/// 2. Once an overflow has recorded event volume, directories with observed
///    volume rank next, heaviest first, ahead of every unobserved candidate.
///    Observed volume is the only direct evidence of which directory fills
///    the kernel queue; an unobserved `node_modules` copy is only a guess.
/// 3. When no candidate has observed event volume, one representative of
///    every enabled ecosystem name ranks before a second copy of any name.
///    This fallback breadth keeps one workspace ecosystem from spending the
///    whole kernel budget on sibling directories. An absent representative is
///    deliberate: watcher backends accept absent exclusions, which begin
///    covering the path if a build creates it later.
/// 4. Representatives follow ecosystem priority. On an exact-path backend the
///    representative is the largest existing copy; on a subtree backend it is
///    the shallowest copy because that path covers its descendants.
/// 5. Remaining candidates preserve the ordinary storm ranking: directories
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
) -> WatcherExclusionPlan {
    derive_exclusion_plan(
        root,
        matcher,
        max_paths,
        crate::watcher_backend::BACKEND_EXCLUSION_COVERAGE,
    )
}

/// The selected exclusions of [`derive_watcher_exclusion_plan`].
#[cfg(test)]
pub(crate) fn derive_excluded_subtrees(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
) -> Vec<WatcherExclusion> {
    derive_watcher_exclusion_plan(root, matcher, max_paths).selected
}

/// [`derive_watcher_exclusion_plan`] for a stated backend coverage.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn derive_exclusion_plan(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
    coverage: WatcherExclusionCoverage,
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
        if depth > WATCHER_NESTED_ECOSYSTEM_MAX_DEPTH {
            return None;
        }
        let name = path.file_name()?.to_str()?;
        if name == ".git" {
            return None;
        }
        ecosystem_exclusion_priority(root, name)
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
    let mut candidates = BTreeMap::<PathBuf, Candidate>::new();
    let mut stack = vec![(root.clone(), 0usize)];

    while let Some((directory, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                continue;
            }
            if exclusion_walk_skips_git_directory(&path) {
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
                directory_size_signal(&path)
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

    if root_git.is_dir() {
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
            if !exclusion_path_is_directory_or_absent(&path) {
                continue;
            }
            match matcher.matched_path_or_any_parents(&path, true) {
                ignore::Match::Ignore(glob) => {
                    let exists = path.is_dir();
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
                    let Ok(entries) = fs::read_dir(&path) else {
                        continue;
                    };
                    for entry in entries.flatten().take(WATCHER_OBSERVED_CHILD_SCAN_CAP) {
                        let child = entry.path();
                        if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
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
                            directory_size_signal(&child)
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
            if !exclusion_path_is_directory_or_absent(&path) {
                continue;
            }
            let ignore::Match::Ignore(glob) = matcher.matched_path_or_any_parents(&path, true)
            else {
                continue;
            };
            let exists = path.is_dir();
            let size_signal = if exists {
                directory_size_signal(&path)
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
    let ecosystem_representatives = ecosystem_representatives
        .into_values()
        .map(|index| candidates[index].relative.clone())
        .collect::<BTreeSet<_>>();

    // The repository's own `.git` keeps slot zero whatever else is known. Every
    // other `.git` is skipped by the walk, so the relative path names it alone.
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

    let limit = max_paths.unwrap_or(usize::MAX);
    let mut selected = Vec::<WatcherExclusion>::new();
    let mut dropped = Vec::<WatcherExclusion>::new();
    for candidate in candidates {
        if coverage == WatcherExclusionCoverage::Subtree
            && candidate.source != WatcherExclusionSource::Observed
        {
            // With inherited coverage the watch walk never descends into an
            // excluded directory, and it skips every ignored directory anyway,
            // so a deeper copy of an already selected name buys nothing and a
            // shallower one is the copy worth naming. On an exact-path backend
            // the same entry is the bug this rule used to cause: the root copy
            // covers nothing, so the nested copy doing the writing never gets
            // a slot.
            let candidate_depth = candidate.relative.components().count();
            let nested_copy = selected.iter().any(|selected| {
                let selected_relative = selected.path.strip_prefix(&root).unwrap_or(&selected.path);
                selected.path.file_name() == candidate.path.file_name()
                    && selected_relative.components().count() < candidate_depth
            });
            if nested_copy {
                continue;
            }
        }
        let exclusion = WatcherExclusion {
            path: candidate.path,
            source: candidate.source,
        };
        if selected.len() < limit {
            selected.push(exclusion);
            continue;
        }
        dropped.push(exclusion);
        if dropped.len() >= WATCHER_DROPPED_CANDIDATE_LOG_LIMIT {
            break;
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
    let repository_key = crate::search_index::artifact_cache_key_memoized_only(root)
        .unwrap_or_else(|| crate::search_index::artifact_cache_key(root));
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
            watcher_path_can_change_corpus_ignore(config, path)
                && !ignore_file_is_ignored_by_matcher(matcher, path)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let ignore_file_changed = !ignore_file_paths.is_empty();

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
        ignore_file_paths,
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
                if absolute == root_git
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

    fn log_overflow(&self, reason: RescanReason) -> bool {
        let prefixes = self.overflow_prefixes();
        self.config
            .counters
            .set_observed_exclusion_prefixes(self.observed_exclusion_prefixes());
        let during_rescan = self.config.counters.note_overflow(reason, prefixes.clone());
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
            "watcher overflow: reason={} root={} exclusions=[{}] candidates_dropped=[{}] matcher_generation={} top_prefixes=[{}] ring_span_ms={} queue_depth={} rescan_in_progress={}",
            reason.as_str(),
            self.config.project_root.display(),
            exclusions,
            candidates_dropped,
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
        let initial = filter_canonical_paths(&self.config, &self.matcher, raw_paths.clone());
        let filtered = if initial.ignore_file_changed {
            let observed_generation = self.matcher_generation.load(Ordering::SeqCst);
            if !self.send_dispatch(WatcherDispatchEvent::IgnoreRulesChanged {
                paths: initial.ignore_file_paths.into_iter().collect(),
            }) {
                return false;
            }
            if !self.wait_for_gitignore_rebuild(observed_generation) {
                return false;
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
    use std::process::Command;
    use tempfile::TempDir;

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
    fn same_name_copies_keep_their_own_slot_only_on_exact_path_backends() {
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
        // Where an exclusion is inherited, the walk that adds watches stops at
        // the root copy, so naming the copies below it would waste slots.
        assert_eq!(
            watcher_exclusion_paths(&subtree.selected),
            vec![canonical_root.join("node_modules")]
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
        assert_eq!(
            dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            WatcherDispatchEvent::RescanRequired(RescanReason::Unknown)
        );
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

        let plan = derive_watcher_exclusion_plan(&canonical_root, &matcher, Some(2));

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
    fn self_ignored_nested_gitignore_rewrites_do_not_report_rule_changes() {
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
            assert!(!filtered.ignore_file_changed);
            assert!(filtered.ignore_file_paths.is_empty());
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
