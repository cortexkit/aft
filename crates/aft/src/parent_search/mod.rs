//! Multi-repo parent folders.
//!
//! A session opened in a folder that is not itself a git checkout but holds
//! git repositories (its children) builds no index of its own. It serves
//! grep, glob, `aft_search`, the call graph tools and inspect from each
//! child's own checkout in the index store (`data_layer::parent`), and from
//! the project-wide inspect results the child's own sessions stored.
//!
//! Everything slow happens on the session's background worker: finding the
//! children and opening the model that embeds queries for them. A request
//! never waits for a child's first build: a child with nothing published yet
//! is a named gap for that child, and files outside every child are a named
//! gap too. Gaps use the existing `complete: false` and `gaps` response
//! fields.
//!
//! The session registry is process-wide and keyed by the canonical parent
//! root, so every context bound to the same parent folder shares one warm set
//! of child readers. The session stops once no context holds it: a context
//! that rebinds to another root, is dropped, or whose routes have all been
//! closed past the unbind grace period no longer counts.

pub(crate) mod engine;
pub(crate) mod external;
pub(crate) use crate::data_layer::parent_query as query;
pub(crate) use crate::data_layer::parent_session::{display_relative, Plane};
pub use crate::data_layer::parent_session::{Discovery, RequestedPlanes};
#[cfg(test)]
mod tests;
mod worker;

pub(crate) use query::attach_search_gaps;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

/// Most child repositories one parent folder serves.
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
    lifecycle: crate::executor::root_lifecycle::SubcLifecycleAdmission,
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
    let Some(planes) = registry.prepared.remove(&root) else {
        if let Some(session) = registry.sessions.get(&root) {
            session.release(context_id(ctx));
        }
        return false;
    };
    if let Some(existing) = registry.sessions.get(&root) {
        // A rebind with the same planes and storage joins the warm session
        // instead of loading every child again.
        if !existing.stopped() && existing.planes == planes && existing.storage == storage {
            existing.hold(Holder::of(ctx));
            return true;
        }
    }
    let session = ParentSession::start(
        root.clone(),
        planes,
        storage.to_path_buf(),
        semantic.clone(),
        Holder::of(ctx),
    );
    crate::data_layer::parent_session::publish(session.clone());
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

/// Failed child view loads per parent folder root. A parent session holds no
/// handle on the contexts bound to it, so its worker records failures here
/// and the parent root's health snapshot reads them by root.
static CHILD_LOAD_FAILURES: std::sync::LazyLock<Mutex<HashMap<PathBuf, ChildLoadFailures>>> =
    std::sync::LazyLock::new(Default::default);

#[derive(Default)]
struct ChildLoadFailures {
    disk_reserve:
        std::collections::BTreeMap<String, crate::view_storage_reserve::DiskReserveCondition>,
    /// Keyed `parent_<plane>:<cause>`, like a context's own view load counts.
    by_cause: std::collections::BTreeMap<String, u64>,
    total: u64,
    last: Option<crate::data_layer::status::ViewLoadFailure>,
    /// Last error-level log per plane and the repeats suppressed since.
    logged: HashMap<&'static str, (Option<Instant>, u64)>,
}

/// Counts a failed child view load under the parent `root` by plane and
/// cause, keeping the full error (prefixed with the child's path) for the
/// parent root's health, and logs it at error level at most once a minute per
/// plane. A busy or locked store, or a child that changed mid-read, clears by
/// itself and the worker retries it, so it is not counted.
fn record_child_failure(
    root: &Path,
    child: &str,
    plane: &'static str,
    error: &str,
    reserve: Option<crate::view_storage_reserve::DiskReserveCondition>,
) {
    if let Some(reserve) = reserve {
        lock(&CHILD_LOAD_FAILURES)
            .entry(root.to_path_buf())
            .or_default()
            .disk_reserve
            .insert(child_reserve_key(child, plane), reserve);
        return;
    }
    let cause = crate::data_layer::status::view_load_cause(error);
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
        failures.last = Some(crate::data_layer::status::ViewLoadFailure {
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
        if entry.0.is_some_and(|last| {
            now.duration_since(last) < crate::data_layer::status::VIEW_LOAD_LOG_INTERVAL
        }) {
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

fn child_reserve_key(child: &str, plane: &str) -> String {
    format!("{plane}/{}", blake3::hash(child.as_bytes()).to_hex())
}

/// The parent session serving `ctx`'s root.
pub fn session_for(ctx: &crate::context::AppContext) -> Option<Arc<ParentSession>> {
    let config = ctx.config();
    session_for_root(config.project_root.as_deref()?)
}

static POINTER_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

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

// ---------------------------------------------------------------------------
// Session and children
// ---------------------------------------------------------------------------

/// One child repository of a parent folder.
pub struct Child {
    view: Arc<crate::data_layer::parent_session::Child>,
    /// The search engine context that runs `aft_search` over this child's
    /// loaded indexes; see [`engine`].
    pub(crate) engine: Mutex<Option<engine::ChildEngine>>,
}

impl Child {
    fn new(parent: &Path, root: PathBuf) -> Self {
        Self {
            view: Arc::new(crate::data_layer::parent_session::Child::new(parent, root)),
            engine: Mutex::new(None),
        }
    }

    /// The child's path relative to the parent, slash-separated, for gaps.
    pub fn display(&self) -> String {
        self.relative.to_string_lossy().replace('\\', "/")
    }

    /// True once this child's search engine serves its loaded indexes.
    pub fn engine_ready(&self) -> bool {
        lock(&self.engine).is_some()
    }
}

impl std::ops::Deref for Child {
    type Target = crate::data_layer::parent_session::Child;

    fn deref(&self) -> &Self::Target {
        &self.view
    }
}

/// The semantic model this session embeds queries with.
pub(crate) struct ParentSemantic {
    /// The model, lent to each child's search engine for the length of one
    /// query so the query is embedded once and no child starts its own model.
    pub model: Mutex<Option<crate::semantic_index::EmbeddingModel>>,
    /// The model's identity, which names the vectors each child's checkout
    /// in the data layer reads (see `data_layer::parent::search_children`).
    pub fingerprint: crate::semantic_index::SemanticIndexFingerprint,
}

/// A parent folder's session state, shared by every context bound to it.
pub struct ParentSession {
    root: PathBuf,
    pub(crate) planes: RequestedPlanes,
    storage: PathBuf,
    pub(crate) semantic_config: crate::config::SemanticBackendConfig,
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
}

impl ParentSession {
    fn start(
        root: PathBuf,
        planes: RequestedPlanes,
        storage: PathBuf,
        semantic_config: crate::config::SemanticBackendConfig,
        holder: Holder,
    ) -> Arc<Self> {
        let session = Arc::new(Self {
            root,
            planes,
            storage,
            semantic_config,
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
    pub(crate) fn note_child_failure(
        &self,
        child: &crate::data_layer::parent_session::Child,
        plane: &'static str,
        error: &str,
    ) -> String {
        let reserve =
            crate::view_storage_reserve::DiskReserveCondition::observed(&self.storage, error);
        let message = reserve
            .as_ref()
            .map_or_else(|| error.to_string(), |reserve| reserve.message.clone());
        record_child_failure(&self.root, &child.display(), plane, error, reserve);
        message
    }

    /// The children found so far; empty until discovery finishes.
    pub fn children(&self) -> Vec<Arc<Child>> {
        read(&self.children).clone()
    }

    /// The finished discovery, or `None` while the worker is still walking.
    pub fn discovery(&self) -> Option<Arc<Discovery>> {
        read(&self.discovery).clone()
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
        crate::data_layer::parent_session::withdraw(self);
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
        crate::data_layer::parent_session::withdraw(self);
    }
}

impl crate::data_layer::parent_session::ParentView for ParentSession {
    fn root(&self) -> &Path {
        self.root()
    }
    fn planes(&self) -> RequestedPlanes {
        self.planes
    }
    fn children(&self) -> Vec<Arc<crate::data_layer::parent_session::Child>> {
        self.children()
            .iter()
            .map(|child| Arc::clone(&child.view))
            .collect()
    }
    fn discovery(&self) -> Option<Arc<Discovery>> {
        self.discovery()
    }
    fn stopped(&self) -> bool {
        self.stopped()
    }
    fn note_child_failure(
        &self,
        child: &crate::data_layer::parent_session::Child,
        plane: &'static str,
        error: &str,
    ) -> String {
        self.note_child_failure(child, plane, error)
    }
}

pub(crate) fn grep_fan_out(
    ctx: &crate::context::AppContext,
    pattern: &crate::pattern_compile::CompiledPattern,
    scope: &crate::grep_executor::GrepScope,
    params: &crate::grep_executor::GrepParams,
    filters: &crate::search_index::PathFilters,
    order: crate::search_index::GrepScanOrder,
) -> Option<(
    crate::search_index::GrepResult,
    crate::grep_executor::GrepExecutionPhaseTimings,
    Vec<serde_json::Value>,
)> {
    let session = session_for(ctx)?;
    query::grep_fan_out(
        ctx,
        session.as_ref(),
        pattern,
        scope,
        params,
        filters,
        order,
    )
}

pub(crate) fn glob_fan_out(
    ctx: &crate::context::AppContext,
    search_roots: &[PathBuf],
    pattern: &str,
) -> Option<query::GlobFanOut> {
    let session = session_for(ctx)?;
    query::glob_fan_out(ctx, session.as_ref(), search_roots, pattern)
}
