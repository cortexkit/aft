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

use std::collections::{BTreeSet, HashMap, HashSet};
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

/// The shared admission probe's default wall-clock budget.
const PROBE_BUDGET: Duration = Duration::from_millis(100);

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

/// What a walk found, and whether it finished.
struct Walk {
    repositories: Vec<PathBuf>,
    outside: Vec<PathBuf>,
}

fn discovery_from_admission(
    root: &Path,
    admission: &crate::folder_admission::FolderAdmission,
    cap: usize,
) -> Discovery {
    let mut found = Walk {
        repositories: Vec::new(),
        outside: Vec::new(),
    };
    let boundary = crate::watcher_filter::WatcherWalkBoundary::for_root(root);
    for candidate in &admission.child_roots {
        let child = std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.clone());
        if !child.starts_with(root) || crate::project_content::is_excluded(&child) {
            continue;
        }
        let hidden = child.strip_prefix(root).ok().is_none_or(|relative| {
            relative
                .components()
                .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
        });
        if hidden {
            continue;
        }
        if boundary.as_ref().is_none_or(|boundary| {
            child
                .ancestors()
                .take_while(|path| path.starts_with(root))
                .any(|path| !boundary.allows_directory(path))
        }) {
            note_outside(&mut found, root, &child);
            continue;
        }
        found.repositories.push(child);
    }
    for path in &admission.outside_paths {
        note_outside(&mut found, root, path);
    }
    if !admission.coverage.workspace_complete {
        // Admission uses routing identities, while discovery returns filesystem
        // canonical paths. Windows can spell those paths with different prefixes.
        let discovered_roots = found
            .repositories
            .iter()
            .filter_map(|child| crate::path_identity::ProjectRootId::from_path(child).ok())
            .collect::<HashSet<_>>();
        for path in &admission.immediate_directories {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !name.starts_with('.')
                && !SKIPPED_DIRECTORY_NAMES.contains(&name)
                && !crate::path_identity::ProjectRootId::from_path(path)
                    .is_ok_and(|identity| discovered_roots.contains(&identity))
            {
                note_outside(&mut found, root, path);
            }
        }
    }
    found.repositories.sort();
    found.repositories.dedup();
    let skipped = found
        .repositories
        .split_off(found.repositories.len().min(cap));
    found.outside.sort();
    Discovery {
        root: root.to_path_buf(),
        children: found.repositories,
        skipped,
        outside: found.outside,
    }
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

/// Bounded workspace classification and child membership for an explicit probe.
pub fn discover(root: &Path, cap: usize) -> Option<Discovery> {
    let root = std::fs::canonicalize(root).ok()?;
    let admission = crate::folder_admission::probe(&root, &Default::default(), Default::default());
    (admission.classification == crate::folder_admission::FolderClassification::Workspace)
        .then(|| discovery_from_admission(&root, &admission, cap))
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
    prepared: HashMap<
        PathBuf,
        (
            RequestedPlanes,
            Discovery,
            crate::folder_admission::ProjectDetectionConfig,
        ),
    >,
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
/// Configure instead passes its deferred admission snapshot to [`prepare_admitted`].
pub fn prepare(root: &Path, planes: RequestedPlanes) -> bool {
    prepare_with_budget(root, planes, PROBE_BUDGET)
}

pub fn prepare_with_budget(root: &Path, planes: RequestedPlanes, budget: Duration) -> bool {
    let admission = crate::folder_admission::probe(
        root,
        &Default::default(),
        crate::folder_admission::ProbeBudgets {
            time: budget,
            ..Default::default()
        },
    );
    prepare_admitted(root, planes, &admission, &Default::default())
}

pub(crate) fn prepare_admitted(
    root: &Path,
    planes: RequestedPlanes,
    admission: &crate::folder_admission::FolderAdmission,
    detection: &crate::folder_admission::ProjectDetectionConfig,
) -> bool {
    if admission.classification != crate::folder_admission::FolderClassification::Workspace {
        return false;
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let discovery = discovery_from_admission(&root, admission, DEFAULT_MAX_CHILD_REPOS);
    lock(registry())
        .prepared
        .insert(root, (planes, discovery, detection.clone()));
    true
}

pub(crate) fn prepared_matches_active(ctx: &crate::context::AppContext, root: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let registry = lock(registry());
    registry
        .prepared
        .get(&root)
        .zip(registry.sessions.get(&root))
        .is_some_and(|((planes, discovery, detection), session)| {
            !session.stopped()
                && session.planes == *planes
                && session.classified == *discovery
                && session.project_detection == *detection
                && lock(&session.holders)
                    .iter()
                    .any(|holder| holder.id == context_id(ctx) && holder.holds())
        })
}

/// Who holds a session: one bound context, tracked without a reference that
/// would keep it alive.
struct Holder {
    workspace_allowed: Arc<AtomicBool>,
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
            workspace_allowed: ctx.folder_admission().workspace_gate(),
            id: context_id(ctx),
            alive: Arc::downgrade(&ctx.configure_generation_flag()),
            lifecycle: ctx.subc_lifecycle_admission(),
        }
    }

    /// True while the context lives and its routes have not all been closed
    /// past the unbind grace period.
    fn holds(&self) -> bool {
        self.workspace_allowed.load(Ordering::Acquire)
            && self.alive.strong_count() > 0
            && !self.lifecycle.unbound_past_grace()
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
    let Some((planes, discovery, detection)) = registry.prepared.remove(&root) else {
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
            && existing.classified == discovery
            && existing.project_detection == detection
        {
            ctx.folder_admission().allow_workspace();
            existing.hold(Holder::of(ctx));
            return true;
        }
    }
    if let Some(existing) = registry.sessions.get(&root) {
        existing.stop();
    }
    ctx.folder_admission().allow_workspace();
    let session = ParentSession::start(
        root.clone(),
        discovery,
        planes,
        storage.to_path_buf(),
        semantic.clone(),
        detection,
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
    session_for_root(root).is_some_and(|session| {
        lock(&session.holders)
            .iter()
            .any(|holder| holder.id == id && holder.holds())
    })
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

pub(crate) fn forget_prepared(root: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    lock(registry()).prepared.remove(&root);
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

/// Failed child view loads per parent folder root. A parent session holds no
/// handle on the contexts bound to it, so its worker records failures here
/// and the parent root's health snapshot reads them by root.
static CHILD_LOAD_FAILURES: std::sync::LazyLock<Mutex<HashMap<PathBuf, ChildLoadFailures>>> =
    std::sync::LazyLock::new(Default::default);

#[derive(Default)]
struct ChildLoadFailures {
    /// Keyed `parent_<plane>:<cause>`, like a context's own view load counts.
    by_cause: std::collections::BTreeMap<String, u64>,
    total: u64,
    last: Option<crate::context::ViewLoadFailure>,
    /// Last error-level log per plane and the repeats suppressed since.
    logged: HashMap<&'static str, (Option<Instant>, u64)>,
}

/// The parent folder's child view load failures for its root health: counts
/// keyed `parent_<plane>:<cause>` and the latest failure. `Some(None)` when
/// none were recorded; `None` when the record is locked right now.
#[allow(clippy::type_complexity)]
pub(crate) fn try_child_load_failures(
    root: &Path,
) -> Option<
    Option<(
        std::collections::BTreeMap<String, u64>,
        Option<crate::context::ViewLoadFailure>,
    )>,
> {
    let failures = match CHILD_LOAD_FAILURES.try_lock() {
        Ok(failures) => failures,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    Some(
        failures
            .get(root)
            .map(|failures| (failures.by_cause.clone(), failures.last.clone())),
    )
}

/// Counts a failed child view load under the parent `root` by plane and
/// cause, keeping the full error (prefixed with the child's path) for the
/// parent root's health, and logs it at error level at most once a minute per
/// plane. A busy or locked store, or a child that changed mid-read, clears by
/// itself and the worker retries it, so it is not counted.
fn record_child_failure(root: &Path, child: &str, plane: &'static str, error: &str) {
    if super::semantic_runtime::is_transient_reason(error) {
        return;
    }
    let cause = crate::context::view_load_cause(error);
    let error = format!("{child}: {error}");
    let now = Instant::now();
    let (count, suppressed) = {
        let mut all = lock(&CHILD_LOAD_FAILURES);
        let failures = all.entry(root.to_path_buf()).or_default();
        let count = {
            let slot = failures
                .by_cause
                .entry(format!("{plane}:{cause}"))
                .or_default();
            *slot = slot.saturating_add(1);
            *slot
        };
        failures.total = failures.total.saturating_add(1);
        failures.last = Some(crate::context::ViewLoadFailure {
            plane: plane.to_string(),
            cause: cause.to_string(),
            error: error.clone(),
            count,
            total: failures.total,
            last_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
        });
        let entry = failures.logged.entry(plane).or_default();
        if entry
            .0
            .is_some_and(|last| now.duration_since(last) < crate::context::VIEW_LOAD_LOG_INTERVAL)
        {
            entry.1 = entry.1.saturating_add(1);
            (count, None)
        } else {
            let suppressed = std::mem::take(&mut entry.1);
            entry.0 = Some(now);
            (count, Some(suppressed))
        }
    };
    if let Some(suppressed) = suppressed {
        crate::slog_error!(
            "parent child view load failed root={} plane={} cause={} failures={} suppressed={} error={}",
            root.display(),
            plane,
            cause,
            count,
            suppressed,
            error
        );
    }
}

/// Test seam for [`record_child_failure`], so tests need no running worker.
#[cfg(test)]
pub(crate) fn record_child_failure_for_test(
    root: &Path,
    child: &str,
    plane: &'static str,
    error: &str,
) {
    record_child_failure(root, child, plane, error);
}

/// The parent session serving `ctx`'s root, when views are on.
pub fn session_for(ctx: &crate::context::AppContext) -> Option<Arc<ParentSession>> {
    let config = ctx.config();
    if !config.views.enabled {
        return None;
    }
    let root = config.project_root.as_deref()?;
    let session = session_for_root(root)?;
    let held = lock(&session.holders)
        .iter()
        .any(|holder| holder.id == context_id(ctx) && holder.holds());
    held.then_some(session)
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
    fn family(&self, session: &ParentSession) -> Result<&str, String> {
        crate::folder_admission_runtime::admit_borrowed(
            &self.root,
            &session.storage,
            &session.project_detection,
        )?;
        if self.family.get().is_none() {
            let key =
                crate::search_index::artifact_cache_key_read_only(&self.root, &session.storage)
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
    lifecycle_bound: bool,
    classified: Discovery,
    project_detection: crate::folder_admission::ProjectDetectionConfig,
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
        classified: Discovery,
        planes: RequestedPlanes,
        storage: PathBuf,
        semantic_config: crate::config::SemanticBackendConfig,
        project_detection: crate::folder_admission::ProjectDetectionConfig,
        trigram_policy: crate::blob_store::v2::TrigramPolicy,
        holder: Holder,
    ) -> Arc<Self> {
        let root_for_backstop = root.clone();
        let session = Arc::new(Self {
            lifecycle_bound: true,
            classified,
            project_detection,
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

    /// Records a child view the worker failed to open or read: an I/O,
    /// verification or store error, not normal coverage such as a view the
    /// child has not published yet. See [`record_child_failure`].
    pub(crate) fn note_child_failure(&self, child: &Child, plane: &'static str, error: &str) {
        record_child_failure(&self.root, &child.display(), plane, error);
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
        self.stop.load(Ordering::SeqCst) || (self.lifecycle_bound && !self.held())
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
