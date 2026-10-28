//! Multi-repo parent folders.
//!
//! A views-on session opened in a folder that is not itself a git checkout
//! but holds git repositories (its children) builds no index of its own. It
//! serves grep, glob, `aft_search`, the call graph tools and inspect from the
//! children's own published indexes, read-only:
//!
//! - **trigram:** the child's published v3 lexical view when its sessions run
//!   with views on, answered as of that publication (the child's own session
//!   is the only one that claims freshness, so the parent applies no file
//!   changes on top). A child whose sessions run with views off publishes
//!   only its persisted `cache.bin`, which the parent opens through the
//!   borrowed read-only loader and keeps current in RAM from its watcher.
//! - **semantic:** the child's published v3 semantic member
//!   (`<scope>.semantic`), opened with read markers on the generation it
//!   serves and the base it composes; answers are as of that publication.
//! - **callgraph:** the child's published v3 call-graph view, protected by a
//!   read marker on the generation it serves. Answers name the publication
//!   they are as of (`as_of`).
//!
//! Everything slow happens on the session's background worker: finding the
//! children, loading each child plane once, following the children's later
//! publications, and applying file changes. Changes reach the worker as
//! paths from the parent's own file watcher, routed to the child that owns
//! them, so only the changed files of that child are re-read; a slow
//! full-reconcile backstop catches anything the watcher missed. A request
//! never loads, builds, repairs or waits for a child index: a child still
//! loading, or without a published index, is a named gap for that child, and
//! files outside every child are a named gap too. Gaps use the existing
//! `complete: false` and `gaps` response fields.
//!
//! The session registry is process-wide and keyed by the canonical parent
//! root, so every context bound to the same parent folder shares one warm set
//! of child readers. The session stops once no context holds it: a context
//! that rebinds to another root, is dropped, or whose routes have all been
//! closed past the unbind grace period no longer counts.

mod engine;
mod inspect;
mod query;
#[cfg(test)]
mod tests;
mod worker;

pub use query::route;
pub(crate) use query::{attach_gaps, glob_fan_out, grep_fan_out};

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use crate::readonly_artifacts::BorrowedArtifactGeneration;
use crate::search_index::SearchIndex;

use super::contracts::ViewAccess;

use super::semantic::SemanticPlane;
use super::snapshot::{OpenGeneration, Snapshot};

/// Most child repositories one parent folder serves. The operator's own
/// projects folder holds 39 repositories, so the cap leaves room above that.
/// Children past the cap are reported as a named gap, never silently dropped.
pub const DEFAULT_MAX_CHILD_REPOS: usize = 64;

/// How deep discovery looks for repositories below the parent folder.
const MAX_DISCOVERY_DEPTH: usize = 3;

/// Upper bound on directory entries discovery examines, so a huge non-project
/// folder cannot keep the worker walking.
const MAX_DISCOVERY_ENTRIES: usize = 20_000;

/// Wall-clock budget of the probe configure runs to decide whether a root is
/// a parent folder. Configure answers a route bind under a deadline, so the
/// probe stops at the first repository it finds, or when this budget ends.
const PROBE_BUDGET: Duration = Duration::from_millis(200);

/// At most this many paths outside every child are remembered for gap reports.
const MAX_OUTSIDE_PATHS: usize = 64;

/// Directory names discovery never descends into: build output and package
/// caches hold no repositories of interest and can be very large.
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "venv",
    "__pycache__",
    "dist",
    "build",
];

/// How often the worker checks the children for new publications (a child
/// session rewriting its trigram artifact or publishing a view generation).
/// Each check reads a few small files per child; file contents are followed
/// through watcher events instead.
const DEFAULT_POINTER_INTERVAL: Duration = Duration::from_secs(10);

/// How often the worker compares each child's files with its trigram
/// snapshot in full, to catch changes the watcher did not deliver. It walks
/// and stats the child; nothing is re-read or copied when nothing changed.
const DEFAULT_BACKSTOP_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The child repositories found below a parent folder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Discovery {
    /// Canonical parent folder.
    pub root: PathBuf,
    /// Served children, canonical, in byte order of their paths.
    pub children: Vec<PathBuf>,
    /// Children past the cap, in the same order. They are not served.
    pub skipped: Vec<PathBuf>,
    /// Top-level entries of the parent folder that hold files outside every
    /// child (at most [`MAX_OUTSIDE_PATHS`]).
    pub outside: Vec<PathBuf>,
}

fn is_repository(dir: &Path) -> bool {
    // A `.git` directory is a primary checkout; a `.git` file is a linked
    // worktree or submodule checkout. Both own their own indexes.
    dir.join(".git").exists()
}

fn home_dir() -> Option<PathBuf> {
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))?;
    let home = PathBuf::from(home);
    Some(std::fs::canonicalize(&home).unwrap_or(home))
}

/// Per-root pause applied to every directory entry discovery examines, so a
/// test can make a walk slow without a slow disk.
fn discovery_delays() -> &'static Mutex<HashMap<PathBuf, Duration>> {
    static DELAYS: OnceLock<Mutex<HashMap<PathBuf, Duration>>> = OnceLock::new();
    DELAYS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Slows discovery under `root` by `delay` per examined entry (zero clears
/// it). Only tests use this.
#[doc(hidden)]
pub fn set_discovery_entry_delay(root: &Path, delay: Duration) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut delays = lock(discovery_delays());
    if delay.is_zero() {
        delays.remove(&root);
    } else {
        delays.insert(root, delay);
    }
}

fn discovery_holds() -> &'static Mutex<HashMap<PathBuf, Arc<DiscoveryHoldState>>> {
    static HOLDS: OnceLock<Mutex<HashMap<PathBuf, Arc<DiscoveryHoldState>>>> = OnceLock::new();
    HOLDS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Default)]
struct DiscoveryHoldState {
    // (worker entered discovery, test released it)
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

/// Holds only the session worker's discovery, not configure's bounded probe.
/// Dropping the guard releases the worker even when an assertion unwinds.
#[doc(hidden)]
pub struct DiscoveryHold {
    root: PathBuf,
    state: Arc<DiscoveryHoldState>,
}

impl DiscoveryHold {
    pub fn wait_until_started(&self, timeout: Duration) -> bool {
        let (state, _) = self
            .state
            .changed
            .wait_timeout_while(lock(&self.state.state), timeout, |state| !state.0)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.0
    }
}

impl Drop for DiscoveryHold {
    fn drop(&mut self) {
        lock(discovery_holds()).remove(&self.root);
        lock(&self.state.state).1 = true;
        self.state.changed.notify_all();
    }
}

/// Pauses background discovery for one root until the returned guard is dropped.
#[doc(hidden)]
pub fn hold_discovery_for_test(root: &Path) -> DiscoveryHold {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let state = Arc::new(DiscoveryHoldState::default());
    assert!(lock(discovery_holds())
        .insert(root.clone(), Arc::clone(&state))
        .is_none());
    DiscoveryHold { root, state }
}

fn wait_for_discovery_hold(root: &Path) {
    let state = lock(discovery_holds()).get(root).cloned();
    if let Some(state) = state {
        let mut held = lock(&state.state);
        held.0 = true;
        state.changed.notify_all();
        while !held.1 {
            held = state.changed.wait(held).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Canonical `root` when it may be a parent folder: not inside a git checkout
/// (which keeps its own indexes) and not the home folder (which indexes
/// nothing).
fn candidate_root(root: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    if root.ancestors().any(is_repository) {
        return None;
    }
    if home_dir().is_some_and(|home| home == root) {
        return None;
    }
    Some(root)
}

/// What a walk found, and whether it finished.
struct Walk {
    repositories: Vec<PathBuf>,
    outside: Vec<PathBuf>,
}

/// Walks `root` for repositories. `stop` is asked after each entry; when it
/// answers true the walk ends with what it found so far.
fn walk(root: &Path, stop: &dyn Fn(&Walk) -> bool) -> Walk {
    let delay = lock(discovery_delays()).get(root).copied();
    let mut found = Walk {
        repositories: Vec::new(),
        outside: Vec::new(),
    };
    let Some(boundary) = crate::watcher_filter::WatcherWalkBoundary::for_root(root) else {
        return found;
    };
    let mut examined = 0usize;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        if !boundary.allows_directory(&dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        let mut subdirectories = Vec::new();
        for entry in entries {
            examined += 1;
            if examined > MAX_DISCOVERY_ENTRIES || stop(&found) {
                return found;
            }
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            let name = entry.file_name();
            let hidden = name.to_str().is_none_or(|name| name.starts_with('.'));
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // Symlinks are not followed: a link to a repository elsewhere
            // would make the parent serve a checkout outside its folder.
            if hidden || kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                if !boundary.allows_directory(&path) {
                    // Another drive or filesystem: never opened, but reported
                    // as unsearched so results over it don't read as complete.
                    note_outside(&mut found, root, &path);
                    continue;
                }
                if name
                    .to_str()
                    .is_some_and(|name| SKIPPED_DIRECTORY_NAMES.contains(&name))
                {
                    continue;
                }
                if is_repository(&path) {
                    found.repositories.push(path);
                } else if depth + 1 < MAX_DISCOVERY_DEPTH {
                    subdirectories.push(path);
                } else {
                    note_outside(&mut found, root, &path);
                }
            } else {
                note_outside(&mut found, root, &path);
            }
        }
        // Depth-first in reverse so directories are visited in name order.
        for subdirectory in subdirectories.into_iter().rev() {
            pending.push((subdirectory, depth + 1));
        }
    }
    found
}

/// Records the top-level entry of `root` that holds `path`: gaps name
/// `docs`, not every file under it.
fn note_outside(found: &mut Walk, root: &Path, path: &Path) {
    let top = path
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .map(|first| root.join(first))
        .unwrap_or_else(|| path.to_path_buf());
    if found.outside.len() < MAX_OUTSIDE_PATHS && !found.outside.contains(&top) {
        found.outside.push(top);
    }
}

/// Finds the child repositories of `root`, keeping at most `cap`.
///
/// `None` means `root` is not a parent folder: it is inside a git checkout,
/// it is the home folder, or it holds no repository within
/// [`MAX_DISCOVERY_DEPTH`].
pub fn discover(root: &Path, cap: usize) -> Option<Discovery> {
    let root = candidate_root(root)?;
    let found = walk(&root, &|_| false);
    if found.repositories.is_empty() {
        return None;
    }
    let mut repositories = found
        .repositories
        .into_iter()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .collect::<Vec<_>>();
    repositories.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    repositories.dedup();
    let skipped = repositories.split_off(repositories.len().min(cap));
    let mut outside = found.outside;
    outside.sort();
    Some(Discovery {
        root,
        children: repositories,
        skipped,
        outside,
    })
}

/// Configure's decision: does `root` hold a repository? Stops at the first
/// one found or after `budget`, whichever comes first. A root whose
/// repositories are not found within the budget keeps its own index.
fn probe(root: &Path, budget: Duration) -> Option<PathBuf> {
    let root = candidate_root(root)?;
    let deadline = Instant::now() + budget;
    let found = walk(&root, &|found| {
        !found.repositories.is_empty() || Instant::now() >= deadline
    });
    (!found.repositories.is_empty()).then_some(root)
}

// ---------------------------------------------------------------------------
// Session registry
// ---------------------------------------------------------------------------

/// The index planes the user asked for before the parent session turned its
/// own indexes off. A plane that is off is not served from the children either.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestedPlanes {
    pub trigram: bool,
    pub semantic: bool,
    pub callgraph: bool,
}

#[derive(Default)]
struct Registry {
    prepared: HashMap<PathBuf, RequestedPlanes>,
    sessions: HashMap<PathBuf, Arc<ParentSession>>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Configure's first step for a views-on root: decides whether `root` is a
/// parent folder and remembers the requested planes for [`activate`]. True
/// means the root must build no index of its own.
///
/// This runs on the configure path, so it only probes: it returns at the
/// first repository found or after a short wall-clock budget. The full walk
/// for every child runs later, on the session worker.
pub fn prepare(root: &Path, planes: RequestedPlanes) -> bool {
    prepare_with_budget(root, planes, PROBE_BUDGET)
}

pub fn prepare_with_budget(root: &Path, planes: RequestedPlanes, budget: Duration) -> bool {
    let Some(root) = probe(root, budget) else {
        return false;
    };
    lock(registry()).prepared.insert(root, planes);
    true
}

/// Who holds a session: one bound context, tracked without a reference that
/// would keep it alive.
struct Holder {
    /// The context's address, unique while the context lives.
    id: usize,
    /// Upgradeable while the context lives: the context owns the only
    /// long-lived strong reference to its configure-generation counter.
    alive: Weak<AtomicU64>,
    lifecycle: crate::context::SubcLifecycleAdmission,
}

impl Holder {
    fn of(ctx: &crate::context::AppContext) -> Self {
        Self {
            id: context_id(ctx),
            alive: Arc::downgrade(&ctx.configure_generation_flag()),
            lifecycle: ctx.subc_lifecycle_admission(),
        }
    }

    /// True while the context lives and its routes have not all been closed
    /// past the unbind grace period.
    fn holds(&self) -> bool {
        self.alive.strong_count() > 0 && !self.lifecycle.unbound_past_grace()
    }
}

fn context_id(ctx: &crate::context::AppContext) -> usize {
    ctx as *const crate::context::AppContext as usize
}

/// Configure's step after it commits to the bind: starts (or joins) the parent session that
/// [`prepare`] decided on and records `ctx` as one of its holders. Returns
/// false when the root was not prepared as a parent folder; `ctx` then stops
/// holding any session for this root.
///
/// Starting a session only spawns its worker; no child is read here.
pub fn activate(
    ctx: &crate::context::AppContext,
    root: &Path,
    storage: &Path,
    semantic: &crate::config::SemanticBackendConfig,
) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut registry = lock(registry());
    let trigram_policy = crate::blob_store::v2::TrigramPolicy {
        max_file_size: ctx.config().search_index_max_file_size,
    };
    let Some(planes) = registry.prepared.remove(&root) else {
        if let Some(session) = registry.sessions.get(&root) {
            session.release(context_id(ctx));
        }
        return false;
    };
    if let Some(existing) = registry.sessions.get(&root) {
        // A rebind with the same planes and storage joins the warm session
        // instead of loading every child again.
        if !existing.stopped()
            && existing.planes == planes
            && existing.storage == storage
            && existing.trigram_policy == trigram_policy
        {
            existing.hold(Holder::of(ctx));
            return true;
        }
    }
    let session = ParentSession::start(
        root.clone(),
        planes,
        storage.to_path_buf(),
        semantic.clone(),
        trigram_policy,
        Holder::of(ctx),
    );
    if let Some(previous) = registry.sessions.insert(root, session) {
        previous.stop();
    }
    true
}

/// True when `ctx` holds the running parent session for `root`.
pub fn held_by(ctx: &crate::context::AppContext, root: &Path) -> bool {
    let id = context_id(ctx);
    session_for_root(root)
        .is_some_and(|session| lock(&session.holders).iter().any(|holder| holder.id == id))
}

/// Called whenever `ctx` binds a root: `ctx` stops holding every parent
/// session other than the one for `root`. A session nobody holds stops.
pub fn release_context_except(ctx: &crate::context::AppContext, root: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let id = context_id(ctx);
    let registry = lock(registry());
    for (session_root, session) in &registry.sessions {
        if *session_root != root {
            session.release(id);
        }
    }
}

/// Release a context as soon as its last route closes. Parent readers have no
/// local builds to preserve during the project-root abandonment grace period.
/// Returns whether the context held a parent session.
pub(crate) fn release_context(ctx: &crate::context::AppContext) -> bool {
    let id = context_id(ctx);
    let mut registry = lock(registry());
    let mut released = false;
    registry.sessions.retain(|_, session| {
        let held = lock(&session.holders).iter().any(|holder| holder.id == id);
        if held {
            released = true;
            session.release(id);
            if !session.held() {
                session.stop();
                return false;
            }
        }
        true
    });
    released
}

/// Drops the parent session for `root`, if any: its worker stops and every
/// reader registration and read marker it holds is released.
pub fn deactivate(root: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut registry = lock(registry());
    registry.prepared.remove(&root);
    if let Some(session) = registry.sessions.remove(&root) {
        session.stop();
    }
}

/// Removes `session` from the registry when it is still the one registered.
fn unregister(session: &ParentSession) {
    let mut registry = lock(registry());
    if registry
        .sessions
        .get(session.root())
        .is_some_and(|registered| std::ptr::eq(Arc::as_ptr(registered), session))
    {
        registry.sessions.remove(session.root());
    }
}

/// The active parent session for `root`.
pub fn session_for_root(root: &Path) -> Option<Arc<ParentSession>> {
    let registry = lock(registry());
    if registry.sessions.is_empty() {
        return None;
    }
    let session = registry.sessions.get(root).cloned().or_else(|| {
        let canonical = std::fs::canonicalize(root).ok()?;
        registry.sessions.get(&canonical).cloned()
    })?;
    (!session.stopped()).then_some(session)
}

/// The parent session serving `ctx`'s root, when views are on.
pub fn session_for(ctx: &crate::context::AppContext) -> Option<Arc<ParentSession>> {
    let config = ctx.config();
    if !config.views.enabled {
        return None;
    }
    session_for_root(config.project_root.as_deref()?)
}

/// Routes changed paths from a file watcher to the parent sessions whose
/// children own them. The watcher thread calls this for every batch it
/// dispatches; it only queues the paths and wakes the session worker.
pub fn note_changed_paths(paths: &[PathBuf]) {
    let sessions = {
        let registry = lock(registry());
        if registry.sessions.is_empty() {
            return;
        }
        registry.sessions.values().cloned().collect::<Vec<_>>()
    };
    for session in sessions {
        let mut woke = false;
        for path in paths.iter().filter(|path| path.starts_with(session.root())) {
            if let Some(child) = session.child_for(path) {
                lock(&child.pending).insert(path.clone());
                woke = true;
            }
        }
        if woke {
            session.wake();
        }
    }
}

static POINTER_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);
/// Always zero: the backstop default is changed per root, never globally.
static BACKSTOP_DEFAULT_MS: AtomicU64 = AtomicU64::new(0);

fn interval(env: &str, overridden: &AtomicU64, default: Duration) -> Duration {
    if let Some(millis) = std::env::var(env)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(millis.max(1));
    }
    match overridden.load(Ordering::SeqCst) {
        0 => default,
        millis => Duration::from_millis(millis),
    }
}

fn store_override(target: &AtomicU64, interval: Duration) {
    target.store(
        u64::try_from(interval.as_millis())
            .unwrap_or(u64::MAX)
            .max(1),
        Ordering::SeqCst,
    );
}

/// Replaces the publication-check interval for sessions started afterwards in
/// this process, so tests can observe child publications without waiting.
#[doc(hidden)]
pub fn set_default_refresh_interval(interval: Duration) {
    store_override(&POINTER_OVERRIDE_MS, interval);
}

/// Replaces the full-reconcile backstop interval for the session next started
/// for `root`. Only tests use this.
#[doc(hidden)]
pub fn set_backstop_interval_for(root: &Path, interval: Duration) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    lock(backstop_overrides()).insert(root, interval);
}

fn backstop_overrides() -> &'static Mutex<HashMap<PathBuf, Duration>> {
    static OVERRIDES: OnceLock<Mutex<HashMap<PathBuf, Duration>>> = OnceLock::new();
    OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn backstop_interval(root: &Path) -> Duration {
    if let Some(interval) = lock(backstop_overrides()).get(root) {
        return *interval;
    }
    interval(
        "AFT_PARENT_BACKSTOP_MS",
        &BACKSTOP_DEFAULT_MS,
        DEFAULT_BACKSTOP_INTERVAL,
    )
}

// ---------------------------------------------------------------------------
// Session and children
// ---------------------------------------------------------------------------

/// One child plane as seen by queries.
pub(crate) enum Plane<T> {
    /// The worker has not finished its first load of this plane.
    Loading,
    Ready(Arc<T>),
    /// The child has nothing this session can serve; the reason is shown to
    /// the user as the gap's reason.
    Gap(String),
}

impl<T> Clone for Plane<T> {
    // Manual so a plane clones its `Arc`, without requiring `T: Clone`.
    fn clone(&self) -> Self {
        match self {
            Self::Loading => Self::Loading,
            Self::Ready(value) => Self::Ready(Arc::clone(value)),
            Self::Gap(reason) => Self::Gap(reason.clone()),
        }
    }
}

impl<T> Plane<T> {
    pub(crate) fn ready(&self) -> Option<&Arc<T>> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    /// The reason this plane cannot answer, for a named gap.
    pub(crate) fn gap_reason(&self, plane: &str) -> Option<String> {
        match self {
            Self::Loading => Some(format!("{plane} index of this repository is still loading")),
            Self::Ready(_) => None,
            Self::Gap(reason) => Some(format!("{plane}: {reason}")),
        }
    }
}

pub(crate) struct TrigramChild {
    pub index: Arc<SearchIndex>,
    source: TrigramSource,
}

/// Where a child's trigram snapshot came from.
#[derive(Clone)]
pub(crate) enum TrigramSource {
    /// The persisted `cache.bin` of a child whose sessions run with views
    /// off. The parent keeps it current from its own watcher and backstop.
    Legacy(BorrowedArtifactGeneration),
    /// The child's published v3 lexical view. Answers are as of the child's
    /// publication ([`PublishedFrontier`]); only the child's own session
    /// claims freshness, so the parent applies no file changes on top.
    View(PublishedChild),
}

/// A v3 generation a child's own session published, as a foreign reader
/// sees it: its name and the frontier its answers are as of.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PublishedChild {
    pub generation: String,
    pub frontier: Option<super::manifest_v2::PublishedFrontier>,
}

impl PublishedChild {
    fn of(generation: &super::snapshot::ComposedGeneration) -> Self {
        Self {
            generation: generation.name().to_owned(),
            frontier: generation.published().cloned(),
        }
    }

    /// The `as_of` object a parent answer carries for this child.
    pub(crate) fn as_of(&self, child: &Child) -> serde_json::Value {
        let frontier = self.frontier.clone().unwrap_or_default();
        serde_json::json!({
            "child": child.display(),
            "generation": self.generation,
            "published_at_ms": frontier.published_at_ms,
            "delta_cut_version": frontier.delta_cut_version,
            "watcher_epoch": frontier.watcher_epoch,
        })
    }
}

pub(crate) struct SemanticChild {
    pub access: ViewAccess,
    pub generation: Arc<OpenGeneration>,
    pub snapshot: Snapshot,
}

pub(crate) struct CallgraphChild {
    pub store: Arc<crate::callgraph_store::ReadonlyCallGraphStore>,
    pub published: PublishedChild,
}

/// One child repository of a parent folder.
pub struct Child {
    /// Canonical checkout root.
    pub root: PathBuf,
    /// Path relative to the parent folder, used as the answer's path prefix.
    pub relative: PathBuf,
    family: OnceLock<String>,
    scope: String,
    pub(crate) trigram: RwLock<Plane<TrigramChild>>,
    pub(crate) semantic: RwLock<Plane<SemanticChild>>,
    pub(crate) callgraph: RwLock<Plane<CallgraphChild>>,
    /// Read-only handle on the child's persisted inspect aggregates, holding
    /// a read marker on the inspect generation it reads.
    pub(crate) inspect: RwLock<Option<Arc<crate::inspect::cache::ReadonlyInspectCache>>>,
    /// The search engine context that runs `aft_search` over this child's
    /// loaded indexes; see [`engine`].
    pub(crate) engine: Mutex<Option<engine::ChildEngine>>,
    /// Changed paths the watcher reported and the worker has not applied.
    pending: Mutex<BTreeSet<PathBuf>>,
    /// Ignore files read for deciding whether a newly created file belongs
    /// to the child, keyed by path with the mtime they were read at.
    ignore_files: Mutex<worker::IgnoreCache>,
    loads: AtomicUsize,
    applied: AtomicUsize,
}

impl Child {
    fn new(parent: &Path, root: PathBuf) -> Self {
        let relative = root
            .strip_prefix(parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| root.clone());
        let scope = crate::path_identity::project_scope_key(&root);
        Self {
            root,
            relative,
            family: OnceLock::new(),
            scope,
            trigram: RwLock::new(Plane::Loading),
            semantic: RwLock::new(Plane::Loading),
            callgraph: RwLock::new(Plane::Loading),
            inspect: RwLock::new(None),
            engine: Mutex::new(None),
            pending: Mutex::new(BTreeSet::new()),
            ignore_files: Mutex::new(worker::IgnoreCache::default()),
            loads: AtomicUsize::new(0),
            applied: AtomicUsize::new(0),
        }
    }

    /// The child's artifact family, the same key its own sessions use.
    fn family(&self, storage: &Path) -> Result<&str, String> {
        if self.family.get().is_none() {
            let key = crate::search_index::artifact_cache_key_read_only(&self.root, storage)
                .map_err(|error| error.to_string())?;
            let _ = self.family.set(key);
        }
        Ok(self.family.get().expect("resolved child family"))
    }

    /// The child's path relative to the parent, slash-separated, for gaps.
    pub fn display(&self) -> String {
        self.relative.to_string_lossy().replace('\\', "/")
    }

    /// How many index artifacts the worker has loaded for this child. Queries
    /// never add to it, and neither does applying file changes.
    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    /// How many changed paths the worker has applied to this child's trigram
    /// snapshot, from watcher events and the backstop together.
    pub fn applied_paths(&self) -> usize {
        self.applied.load(Ordering::SeqCst)
    }

    pub fn trigram_ready(&self) -> bool {
        read(&self.trigram).ready().is_some()
    }

    pub fn semantic_ready(&self) -> bool {
        read(&self.semantic).ready().is_some()
    }

    pub fn callgraph_ready(&self) -> bool {
        read(&self.callgraph).ready().is_some()
    }

    /// True once this child's search engine serves its loaded indexes.
    pub fn engine_ready(&self) -> bool {
        lock(&self.engine).is_some()
    }

    /// The callgraph generation this session holds a read marker on.
    pub fn callgraph_generation(&self) -> Option<String> {
        read(&self.callgraph)
            .ready()
            .map(|callgraph| callgraph.published.generation.clone())
    }

    /// The semantic generation this session holds a read marker on.
    pub fn semantic_generation(&self) -> Option<String> {
        read(&self.semantic)
            .ready()
            .map(|semantic| semantic.generation.name().to_owned())
    }
}

/// The semantic model this session embeds queries with, and the shared
/// semantic plane whose arena holds the children's vectors once per family.
pub(crate) struct ParentSemantic {
    /// The model, lent to each child's search engine for the length of one
    /// query so the query is embedded once and no child starts its own model.
    pub model: Mutex<Option<crate::semantic_index::EmbeddingModel>>,
    pub plane: Arc<SemanticPlane>,
    pub producer_id: String,
}

/// A parent folder's session state, shared by every context bound to it.
pub struct ParentSession {
    root: PathBuf,
    pub(crate) planes: RequestedPlanes,
    storage: PathBuf,
    semantic_config: crate::config::SemanticBackendConfig,
    /// The trigram policy of the children's v3 views: the producer their
    /// lexical and call-graph members are published under.
    trigram_policy: crate::blob_store::v2::TrigramPolicy,
    /// Parent call-graph and lexical answers served from children's v3 views.
    view_routes: [AtomicU64; 2],
    /// `None` until the worker's first full discovery walk finishes.
    discovery: RwLock<Option<Arc<Discovery>>>,
    children: RwLock<Vec<Arc<Child>>>,
    pub(crate) semantic: RwLock<Plane<ParentSemantic>>,
    holders: Mutex<Vec<Holder>>,
    stop: AtomicBool,
    /// While set, the worker skips its rounds, so a test can hold the
    /// generations the session currently serves.
    paused: AtomicBool,
    /// Completed publication-check rounds, including the first load.
    rounds: AtomicUsize,
    wake: (Mutex<bool>, Condvar),
    pointer_interval: Duration,
    backstop_interval: Duration,
}

impl ParentSession {
    fn start(
        root: PathBuf,
        planes: RequestedPlanes,
        storage: PathBuf,
        semantic_config: crate::config::SemanticBackendConfig,
        trigram_policy: crate::blob_store::v2::TrigramPolicy,
        holder: Holder,
    ) -> Arc<Self> {
        let root_for_backstop = root.clone();
        let session = Arc::new(Self {
            root,
            planes,
            storage,
            semantic_config,
            trigram_policy,
            view_routes: Default::default(),
            discovery: RwLock::new(None),
            children: RwLock::new(Vec::new()),
            semantic: RwLock::new(if planes.semantic {
                Plane::Loading
            } else {
                Plane::Gap("semantic search is off".into())
            }),
            holders: Mutex::new(vec![holder]),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            rounds: AtomicUsize::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            pointer_interval: interval(
                "AFT_PARENT_REFRESH_MS",
                &POINTER_OVERRIDE_MS,
                DEFAULT_POINTER_INTERVAL,
            ),
            backstop_interval: backstop_interval(&root_for_backstop),
        });
        let weak = Arc::downgrade(&session);
        let spawned = std::thread::Builder::new()
            .name("aft-parent-folder".into())
            .spawn(move || worker::run(weak));
        if let Err(error) = spawned {
            crate::slog_warn!("parent folder worker could not start: {}", error);
            session.stop();
        }
        session
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The children found so far; empty until discovery finishes.
    pub fn children(&self) -> Vec<Arc<Child>> {
        read(&self.children).clone()
    }

    /// The finished discovery, or `None` while the worker is still walking.
    pub fn discovery(&self) -> Option<Arc<Discovery>> {
        read(&self.discovery).clone()
    }

    /// Parent answers served from the children's v3 views, ordered as call
    /// graph, then grep and glob.
    pub fn view_route_counts(&self) -> [u64; 2] {
        std::array::from_fn(|index| self.view_routes[index].load(Ordering::Relaxed))
    }

    pub(crate) fn note_view_route(&self, index: usize) {
        self.view_routes[index].fetch_add(1, Ordering::Relaxed);
    }

    /// Completed worker rounds; the first one is the initial load.
    pub fn rounds(&self) -> usize {
        self.rounds.load(Ordering::SeqCst)
    }

    /// Waits until the worker finished `rounds` rounds, for tests and tools
    /// that need a settled session. Queries never call this.
    pub fn wait_rounds(&self, rounds: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.rounds() < rounds {
            if Instant::now() >= deadline || self.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// True once the session stopped: no context holds it any more, or it was
    /// replaced or deactivated.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Holds (or releases) the worker's rounds. Queries keep being answered
    /// from what is already loaded.
    #[doc(hidden)]
    pub fn pause_refresh(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        self.wake();
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
    }

    fn wake(&self) {
        let (flag, condvar) = &self.wake;
        *lock(flag) = true;
        condvar.notify_all();
    }

    /// Waits up to `timeout` for a wake-up; true when one arrived.
    fn wait_for_wake(&self, timeout: Duration) -> bool {
        let (flag, condvar) = &self.wake;
        let mut woken = lock(flag);
        if !*woken {
            woken = condvar
                .wait_timeout(woken, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        std::mem::replace(&mut *woken, false)
    }

    fn hold(&self, holder: Holder) {
        let mut holders = lock(&self.holders);
        holders.retain(|existing| existing.id != holder.id);
        holders.push(holder);
    }

    fn release(&self, id: usize) {
        lock(&self.holders).retain(|holder| holder.id != id);
        self.wake();
    }

    /// True while some context still holds this session.
    fn held(&self) -> bool {
        let mut holders = lock(&self.holders);
        holders.retain(Holder::holds);
        !holders.is_empty()
    }

    /// The child whose checkout contains `path`.
    pub fn child_for(&self, path: &Path) -> Option<Arc<Child>> {
        read(&self.children)
            .iter()
            .filter(|child| path.starts_with(&child.root))
            .max_by_key(|child| child.root.components().count())
            .cloned()
    }
}

impl Drop for ParentSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Paths in `paths` relative to the parent root, for gap reports.
pub(crate) fn display_relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let text = relative.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".into()
    } else {
        text
    }
}
