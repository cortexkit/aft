//! Views-on semantic search for one checkout.
//!
//! With `views.enabled`, a root's semantic lane is served by its own
//! per-checkout view instead of the resident legacy index: a
//! [`CheckoutSemantic`] owns the checkout's registration in its family, the
//! composite [`CheckoutDriver`] with the [`SemanticPlane`] registered as its
//! only plane, and the sibling loader that installs the checkout's
//! generation. Identical content is embedded once per family, so worktrees of
//! one repository (and later sessions on them) reuse each other's vectors
//! instead of calling the model again.
//!
//! The trigram and callgraph planes are not registered here: their views-on
//! paths keep using the older content-addressed view, and their per-checkout
//! activation is separate work. The manifest header therefore names them with
//! [`UNREGISTERED_PRODUCER`], which no registered plane ever produces.
//!
//! Loading, filling and folding run on the root's semantic view worker, never
//! on the request path. Queries read the installed snapshot and name every
//! path without current vectors as a gap.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use crate::semantic_index::SemanticIndex;

use super::contracts::{PlaneAdapter, PlaneLoader, ViewAccess};
use super::first_load::{
    CheckoutDriver, CompositePlane, ConfiguredMembershipWalker, InstalledDriver, SiblingLoader,
};
use super::intent::{WriteIntent, WriteIntentListener, WritePhase};
use super::manifest_v2::Producers;
use super::registry::{FamilyRegistry, ViewRegistration};
use super::semantic::{FillBudget, FillReport, SemanticPlane, SemanticProducer, SemanticQuery};
use super::snapshot::Snapshot;

/// Producer written into the manifest header for the trigram and callgraph
/// planes, which this semantic-only runtime does not register.
pub const UNREGISTERED_PRODUCER: &str = "unregistered";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The process's semantic plane for `storage` and `producer`.
///
/// Every checkout of this process that uses the same storage root and
/// embedding producer gets the same plane, so their family arenas, resident
/// generations and single-flight embedding claims are shared. The registry
/// holds weak references: when the last checkout releases a plane, its
/// resident vectors are freed.
pub fn shared_plane(storage: &Path, producer: SemanticProducer) -> Arc<SemanticPlane> {
    type Registry = Vec<(PathBuf, String, Weak<SemanticPlane>)>;
    static PLANES: OnceLock<Mutex<Registry>> = OnceLock::new();
    let mut planes = lock(PLANES.get_or_init(|| Mutex::new(Vec::new())));
    planes.retain(|(_, _, plane)| plane.strong_count() > 0);
    let id = producer.id();
    if let Some(plane) = planes
        .iter()
        .find(|(root, producer, _)| root == storage && *producer == id)
        .and_then(|(_, _, plane)| plane.upgrade())
    {
        return plane;
    }
    let plane = Arc::new(SemanticPlane::new(storage.to_path_buf(), producer));
    planes.push((storage.to_path_buf(), id, Arc::downgrade(&plane)));
    plane
}

/// One checkout's semantic view: registration, driver, loader and plane.
pub struct CheckoutSemantic {
    root: PathBuf,
    owner: ViewRegistration,
    access: ViewAccess,
    driver: Arc<CheckoutDriver>,
    loader: SiblingLoader,
    plane: Arc<SemanticPlane>,
    /// Serializes reload, fill and fold, so two refreshes never interleave
    /// their publications.
    refresh: Mutex<()>,
    /// Fills installed since the last fold into a published generation.
    unfolded: AtomicBool,
    refresh_attempts: AtomicUsize,
    /// Why this checkout's vectors cannot be brought up to date, including
    /// the scheduled deadline when a transient refresh is being retried. Searches
    /// report it as `semantic: unavailable: <reason>`.
    unavailable: Mutex<Option<String>>,
    /// Wakes the fill worker after an AFT write to this checkout. It is held
    /// here because the intent registry keeps only weak references.
    _wake_on_write: Arc<dyn WriteIntentListener>,
}

impl CheckoutSemantic {
    /// Registers `root` as view `scope` of `family` and builds its driver.
    /// Nothing is walked or published until [`Self::load`].
    ///
    /// `wake` is told about AFT writes under `root`; pass `Weak::new()` when
    /// nothing needs waking.
    pub fn new(
        storage: &Path,
        family: &str,
        scope: &str,
        root: &Path,
        producer: SemanticProducer,
        wake: Weak<CheckoutSemanticSlot>,
    ) -> Result<Self, String> {
        let plane = shared_plane(storage, producer);
        let registry = FamilyRegistry::open(storage, family).map_err(|error| error.to_string())?;
        let owner = registry
            .register_view(scope, root)
            .map_err(|error| error.to_string())?;
        let composite: Arc<dyn CompositePlane> = plane.clone();
        let adapter: Arc<dyn PlaneAdapter> = plane.clone();
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: UNREGISTERED_PRODUCER.into(),
                    semantic: Some(plane.semantic_producer().id()),
                    callgraph: UNREGISTERED_PRODUCER.into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![composite],
            )
            .with_adapters(vec![adapter.clone()]),
        );
        let loader = SiblingLoader::new(driver.clone(), vec![adapter]);
        let listener: Arc<dyn WriteIntentListener> = Arc::new(WakeOnWrite {
            root: root.to_path_buf(),
            slot: wake,
        });
        super::intent::register_listener(&listener);
        let access = ViewAccess::Owner(owner.clone());
        hold_view(plane.as_ref(), &access);
        Ok(Self {
            root: root.to_path_buf(),
            access,
            owner,
            driver,
            loader,
            plane,
            refresh: Mutex::new(()),
            unfolded: AtomicBool::new(false),
            refresh_attempts: AtomicUsize::new(0),
            unavailable: Mutex::new(None),
            _wake_on_write: listener,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn access(&self) -> &ViewAccess {
        &self.access
    }

    pub fn driver(&self) -> &Arc<CheckoutDriver> {
        &self.driver
    }

    pub fn plane(&self) -> &Arc<SemanticPlane> {
        &self.plane
    }

    /// The snapshot queries currently read.
    pub fn installed(&self) -> Snapshot {
        self.driver.installed_snapshot()
    }

    /// Strictly reconciles the checkout, publishes its own generation (which
    /// folds every installed fill) and installs it.
    pub fn load(&self) -> Result<Snapshot, String> {
        self.loader
            .load(&self.access)
            .map(|outcome| outcome.snapshot)
            .map_err(|error| error.to_string())
    }

    /// Brings the checkout's vectors up to date within `budget`.
    ///
    /// The checkout is reconciled first when the installed snapshot carries
    /// edits it has not reconciled (a watcher change or an AFT write records
    /// an intent), so the fill sees the current bytes. Once no budgeted work
    /// remains (or a round makes no progress), the completions are folded
    /// into a newly published generation, which keeps them across restarts
    /// and lets sibling checkouts seed from them. Folding once per catch-up
    /// rather than per round avoids repeated manifest publications. A fold
    /// reuses reconciled disk facts unless an edit arrives during publication.
    /// Until then the fill map serves them and the live pin protects them.
    ///
    /// Errors are classified: [`RefreshError::Transient`] for failures that
    /// can clear by themselves (see [`is_transient_reason`]), and
    /// [`RefreshError::Unavailable`] for everything else. A fill that could
    /// not store what it embedded reports the store errors in its report.
    pub fn refresh<F>(&self, budget: FillBudget, embed: &mut F) -> Result<FillReport, RefreshError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        let _serial = lock(&self.refresh);
        self.refresh_attempts.fetch_add(1, Ordering::SeqCst);
        let mut snapshot = self.installed();
        if needs_reload(&snapshot) {
            snapshot = if snapshot.generation().name() == "empty" {
                self.load().map_err(RefreshError::classify)?
            } else {
                self.loader
                    .refresh(&self.access, Arc::clone(snapshot.generation()))
                    .map_err(|error| RefreshError::classify(error.to_string()))?
            };
        }
        let driver = Arc::clone(&self.driver);
        let report = self
            .plane
            .fill(&self.owner, &snapshot, budget, embed, &move || {
                driver.installed_snapshot()
            })
            .map_err(|error| RefreshError::classify(error.to_string()))?;
        if report.installed > 0 || report.failed > 0 {
            self.unfolded.store(true, Ordering::SeqCst);
        }
        let caught_up = report.deferred == 0 || (report.installed == 0 && report.failed == 0);
        if caught_up && self.unfolded.swap(false, Ordering::SeqCst) {
            crate::cold_build_limiter::progress::phase("publishing", None);
            let (snapshot, revision) = self.driver.installed_cut();
            if let Err(error) = self.loader.fold(&self.access, &snapshot, revision) {
                self.unfolded.store(true, Ordering::SeqCst);
                return Err(RefreshError::classify(error.to_string()));
            }
        }
        Ok(report)
    }

    /// How many times `refresh` has run, successful or not.
    pub fn refresh_attempts(&self) -> usize {
        self.refresh_attempts.load(Ordering::SeqCst)
    }

    /// Why the checkout's vectors cannot be brought up to date, including a
    /// scheduled retry deadline for transient errors.
    pub fn unavailable_reason(&self) -> Option<String> {
        lock(&self.unavailable).clone()
    }

    /// Stores why the checkout's vectors cannot be brought up to date. True
    /// when the reason changed, so the caller logs it once rather than on
    /// every attempt.
    fn set_unavailable(&self, reason: String) -> bool {
        let mut unavailable = lock(&self.unavailable);
        let changed = unavailable.as_deref() != Some(reason.as_str());
        *unavailable = Some(reason);
        changed
    }

    fn clear_unavailable(&self) {
        *lock(&self.unavailable) = None;
    }

    /// Scores `query_vector` against the installed snapshot.
    pub fn search(
        &self,
        query_vector: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<SemanticQuery, String> {
        let mut answer = self
            .plane
            .search(
                &self.access,
                &self.root,
                &self.installed(),
                query_vector,
                top_k,
                include,
            )
            .map_err(|error| error.to_string())?;
        answer.unavailable = self.unavailable_reason();
        Ok(answer)
    }

    /// The resident index `search` scores for the installed snapshot, for
    /// callers (such as the search engine's readiness snapshot) that hold it.
    /// Builds it when the cached one is out of date.
    pub fn index(&self) -> Result<Arc<SemanticIndex>, String> {
        self.plane
            .overlay(&self.access, &self.root, &self.installed())
            .map(|overlay| overlay.index)
            .map_err(|error| error.to_string())
    }

    /// The index this checkout last built, possibly for an older snapshot,
    /// without building anything. Readiness reports use it: they run on the
    /// search path and only need to know that an index is being served.
    pub fn cached_index(&self) -> Option<Arc<SemanticIndex>> {
        self.plane.cached_index(&self.access)
    }

    /// Resident bytes this checkout accounts for: the family arena (shared
    /// by every checkout of the family in this process, counted once there)
    /// and this checkout's own overlay.
    pub fn memory(&self) -> CheckoutSemanticMemory {
        CheckoutSemanticMemory {
            family_arena: self.plane.arena(self.owner.family()).memory().bytes,
            private: self.plane.private_memory(&self.access),
        }
    }
}

impl Drop for CheckoutSemantic {
    fn drop(&mut self) {
        // A lane restarted with the same producer shares this plane and view
        // (same family and scope). Only the last checkout runtime of a view
        // releases its state, or a superseded worker exiting would wipe the
        // fills and resident generations of the lane that replaced it.
        if release_view(self.plane.as_ref(), &self.access) {
            self.plane.unbind(&self.access);
        }
    }
}

/// A view of one plane: the plane's process-unique id (never reused, unlike
/// its address after it is freed), the family and the scope.
type LiveViewKey = (u64, String, String);

fn live_views() -> &'static Mutex<std::collections::HashMap<LiveViewKey, usize>> {
    static LIVE: OnceLock<Mutex<std::collections::HashMap<LiveViewKey, usize>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

fn live_view_key(plane: &SemanticPlane, access: &ViewAccess) -> LiveViewKey {
    (
        plane.id(),
        access.family().to_owned(),
        access.scope().to_owned(),
    )
}

/// How many live `CheckoutSemantic`s hold the view `access` names on `plane`.
pub fn live_holders(plane: &SemanticPlane, access: &ViewAccess) -> usize {
    lock(live_views())
        .get(&live_view_key(plane, access))
        .copied()
        .unwrap_or(0)
}

fn hold_view(plane: &SemanticPlane, access: &ViewAccess) {
    *lock(live_views())
        .entry(live_view_key(plane, access))
        .or_default() += 1;
}

/// Drops one holder of the view; true when it was the last.
fn release_view(plane: &SemanticPlane, access: &ViewAccess) -> bool {
    let mut live = lock(live_views());
    let key = live_view_key(plane, access);
    let Some(count) = live.get_mut(&key) else {
        // Every `CheckoutSemantic::new` holds exactly once and every drop
        // releases exactly once, so a missing entry is a bookkeeping bug.
        debug_assert!(false, "released a semantic view that was never held");
        return true;
    };
    *count -= 1;
    if *count == 0 {
        live.remove(&key);
        true
    } else {
        false
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CheckoutSemanticMemory {
    pub family_arena: u64,
    pub private: u64,
}

/// Whether the installed snapshot has edits a strict reconcile must resolve
/// before its vectors can be brought up to date. The initial snapshot (before
/// any generation was published) always needs one.
fn needs_reload(snapshot: &Snapshot) -> bool {
    snapshot.generation().name() == "empty"
        || snapshot.pending_intent().next().is_some()
        || snapshot.live_entries().next().is_some()
}

/// Wakes the fill worker after an AFT write lands under the checkout root.
/// The driver records the write intent itself; this only schedules the
/// refresh that resolves it.
struct WakeOnWrite {
    root: PathBuf,
    slot: Weak<CheckoutSemanticSlot>,
}

impl WriteIntentListener for WakeOnWrite {
    fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
        if phase != WritePhase::After {
            return;
        }
        let touches = match change {
            WriteIntent::Paths(paths) => paths.iter().any(|path| path.starts_with(&self.root)),
            WriteIntent::Directory(directory) => {
                directory.starts_with(&self.root) || self.root.starts_with(directory)
            }
        };
        if touches {
            if let Some(slot) = self.slot.upgrade() {
                slot.wake();
            }
        }
    }
}

/// What a root's views-on semantic lane currently is.
#[derive(Clone)]
pub enum CheckoutSemanticState {
    /// The worker is starting the model or loading the checkout.
    Loading,
    Ready(Arc<CheckoutSemantic>),
    /// Registration or loading failed; the reason is reported as the
    /// semantic lane's named cause, never replaced by another index.
    Unavailable(String),
}

/// A root's views-on semantic lane, shared between its context and its fill
/// worker. Each start takes a new epoch; a worker whose epoch is no longer
/// current stops and never installs its result.
#[derive(Default)]
pub struct CheckoutSemanticSlot {
    state: RwLock<Option<CheckoutSemanticState>>,
    epoch: AtomicU64,
    wake: Mutex<Option<crossbeam_channel::Sender<()>>>,
    /// Worker threads currently running for this slot, of any epoch.
    workers: AtomicUsize,
    /// Held while an epoch changes and while a lane publishes its view, so a
    /// publication and the check that its lane is still current are atomic.
    publish: Mutex<()>,
    retry: Mutex<Option<RetryDeadline>>,
}

struct RetryDeadline {
    due: Instant,
    query_allowed_at: Instant,
}

impl CheckoutSemanticSlot {
    /// Worker threads of this lane still running, including superseded ones
    /// that have not noticed yet.
    pub fn workers(&self) -> usize {
        self.workers.load(Ordering::SeqCst)
    }

    /// One line describing the lane, for diagnostics.
    pub fn describe(&self) -> String {
        let state = match self.state() {
            None => "none".to_owned(),
            Some(CheckoutSemanticState::Loading) => "loading".to_owned(),
            Some(CheckoutSemanticState::Ready(runtime)) => match runtime.unavailable_reason() {
                Some(reason) => format!("unavailable: {reason}"),
                None => format!(
                    "ready producer={}",
                    runtime.plane().semantic_producer().id()
                ),
            },
            Some(CheckoutSemanticState::Unavailable(reason)) => format!("unavailable: {reason}"),
        };
        format!(
            "{state} epoch={} workers={}",
            self.epoch.load(Ordering::SeqCst),
            self.workers()
        )
    }

    /// Starts a new epoch in the `Loading` state and returns it with the
    /// receiver the new worker waits on.
    pub fn begin(&self) -> (u64, crossbeam_channel::Receiver<()>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        // Under the publish lock: once this returns, no earlier epoch's
        // publication is in flight or can start (see `LaneFence`).
        let epoch = {
            let _publishing = lock(&self.publish);
            self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
        };
        *lock(&self.wake) = Some(sender);
        *lock(&self.retry) = None;
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(CheckoutSemanticState::Loading);
        (epoch, receiver)
    }

    pub fn is_current(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) == epoch
    }

    fn set(&self, epoch: u64, state: CheckoutSemanticState) -> bool {
        let mut slot = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.is_current(epoch) {
            return false;
        }
        *slot = Some(state);
        true
    }

    pub fn install(&self, epoch: u64, runtime: Arc<CheckoutSemantic>) -> bool {
        self.set(epoch, CheckoutSemanticState::Ready(runtime))
    }

    pub fn fail(&self, epoch: u64, reason: String) -> bool {
        self.set(epoch, CheckoutSemanticState::Unavailable(reason))
    }

    /// Forgets the lane. Its worker sees the channel close and exits.
    pub fn clear(&self) {
        {
            let _publishing = lock(&self.publish);
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
        *lock(&self.wake) = None;
        *lock(&self.retry) = None;
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub fn state(&self) -> Option<CheckoutSemanticState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether a views-on semantic lane was started for the root, in any
    /// state. While it is, the legacy semantic index is not built.
    pub fn active(&self) -> bool {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    pub fn runtime(&self) -> Option<Arc<CheckoutSemantic>> {
        match self.state()? {
            CheckoutSemanticState::Ready(runtime) => Some(runtime),
            _ => None,
        }
    }

    /// Asks the worker to refresh after its quiet window.
    pub fn wake(&self) {
        if let Some(sender) = lock(&self.wake).as_ref() {
            let _ = sender.send(());
        }
    }

    /// Expedites a transient retry at most once per probe interval.
    /// The request path only wakes the worker; it never loads or embeds sources.
    pub fn request_retry(&self) {
        if self.request_retry_at(Instant::now()) {
            self.wake();
        }
    }

    fn request_retry_at(&self, now: Instant) -> bool {
        let mut retry = lock(&self.retry);
        let Some(retry) = retry.as_mut() else {
            return false;
        };
        if now < retry.query_allowed_at || now >= retry.due {
            return false;
        }
        retry.due = now;
        true
    }
}

/// Everything the fill worker needs besides the slot.
pub(crate) struct WorkerConfig {
    pub root: PathBuf,
    pub storage: PathBuf,
    pub family: String,
    pub scope: String,
    pub semantic: crate::config::SemanticBackendConfig,
    /// The root's semantic status, which every status and readiness surface
    /// reads. The worker keeps it in step with the lane.
    pub status: Arc<RwLock<crate::context::SemanticIndexStatus>>,
    /// The root's installed checkout driver, which receives watcher changes.
    pub drivers: InstalledDriver,
    pub schedule: FillSchedule,
}

/// When and under what admission the worker fills.
pub(crate) struct FillSchedule {
    pub root: PathBuf,
    /// How long wakes must stop arriving before a fill runs, so a burst of
    /// edits is embedded once.
    pub quiet_window: Duration,
    /// First and longest wait before retrying a fill that left work behind
    /// because of a transient error (an unreachable or failing embedding
    /// backend, a checkout that changed during reconciliation). The wait
    /// doubles after each retry that makes no progress and starts over after
    /// one that does, so a backend that comes back is used again without
    /// any edit or reconfigure.
    pub retry_initial: Duration,
    pub retry_max: Duration,
    pub limiter: Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    /// True while the root has been unbound past the abandon grace window;
    /// refreshes wait for a rebind instead of loading the embedder.
    pub paused: Box<dyn Fn() -> bool + Send>,
    pub max_batch: usize,
}

/// Production retry waits for fills that hit a transient error.
pub(crate) const RETRY_INITIAL: Duration = Duration::from_secs(5);
pub(crate) const RETRY_MAX: Duration = Duration::from_secs(300);

const LIMITER_KIND: &str = "semantic view fill";

/// Lets a lane's driver publish its checkout's view only while the lane's
/// epoch is current. The check and the pointer publication run under the
/// slot's publish lock, which every epoch change also takes, so a worker
/// superseded by a restart (a reconfigure with another model) can never
/// publish its old producer's manifest over the replacement's: readers of
/// the view never see the old producer again once the new lane starts.
pub struct LaneFence {
    slot: Weak<CheckoutSemanticSlot>,
    epoch: u64,
}

impl LaneFence {
    pub fn new(slot: Weak<CheckoutSemanticSlot>, epoch: u64) -> Self {
        Self { slot, epoch }
    }
}

impl super::first_load::PublishFence for LaneFence {
    fn publish(
        &self,
        commit: &mut dyn FnMut() -> Result<(), super::contracts::PlaneError>,
    ) -> Result<(), super::contracts::PlaneError> {
        let refused = || {
            crate::slog_debug!(
                "semantic view publication refused for superseded lane epoch {}",
                self.epoch
            );
            Err(super::contracts::PlaneError {
                plane: crate::blob_store::v2::FamilyPlane::Semantic,
                reason: super::first_load::SUPERSEDED_PUBLISH.to_owned(),
            })
        };
        let Some(slot) = self.slot.upgrade() else {
            return refused();
        };
        let _publishing = lock(&slot.publish);
        if !slot.is_current(self.epoch) {
            return refused();
        }
        commit()
    }
}

fn set_status(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    status: &RwLock<crate::context::SemanticIndexStatus>,
    value: crate::context::SemanticIndexStatus,
) {
    if slot.upgrade().is_some_and(|slot| slot.is_current(epoch)) {
        *status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }
}

fn is_current(slot: &Weak<CheckoutSemanticSlot>, epoch: u64) -> bool {
    slot.upgrade().is_some_and(|slot| slot.is_current(epoch))
}

/// Arms one bounded retry and names its deadline on every status surface.
fn schedule_retry(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    reason: &str,
    wait: Duration,
    initial: Duration,
) -> String {
    let now = Instant::now();
    if let Some(slot) = slot.upgrade() {
        let mut retry = lock(&slot.retry);
        // Recheck after acquiring the mutex: an epoch can change while a
        // superseded worker waits for the replacement's retry state.
        if slot.is_current(epoch) {
            *retry = Some(RetryDeadline {
                due: now + wait,
                query_allowed_at: now + initial.min(Duration::from_secs(1)),
            });
        }
    }
    let due = (std::time::SystemTime::now() + wait)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "{}; next retry due at unix_ms={due} (scheduled in {} ms)",
        crate::semantic_index::strip_transient_embedding_marker(reason),
        wait.as_millis()
    )
}

fn clear_retry(slot: &Weak<CheckoutSemanticSlot>, epoch: u64) {
    if let Some(slot) = slot.upgrade() {
        let mut retry = lock(&slot.retry);
        if slot.is_current(epoch) {
            *retry = None;
        }
    }
}

/// Ordinary edit wakes do not bypass backoff. Only a rate-limited query can
/// advance the deadline. Channel closure or an epoch change cancels the wait.
fn wait_retry(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
) -> bool {
    loop {
        let Some(current) = slot.upgrade().filter(|slot| slot.is_current(epoch)) else {
            return false;
        };
        let remaining = lock(&current.retry)
            .as_ref()
            .map(|retry| retry.due.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        drop(current);
        if remaining.is_zero() {
            return true;
        }
        match wake.recv_timeout(remaining) {
            Ok(()) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => return is_current(slot, epoch),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Retry the failed stage, not the whole activation. In particular, a failed
/// fingerprint probe cannot cause a checkout walk, read or content embedding.
fn retry_activation<T>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
    config: &WorkerConfig,
    stage: &str,
    mut attempt: impl FnMut() -> Result<T, String>,
) -> Result<T, RefreshError> {
    let mut backoff = config.schedule.retry_initial;
    loop {
        if !is_current(slot, epoch) {
            return Err(RefreshError::Superseded);
        }
        match attempt().map_err(RefreshError::classify) {
            Ok(value) => {
                clear_retry(slot, epoch);
                return Ok(value);
            }
            Err(RefreshError::Transient(reason)) => {
                let reason = schedule_retry(
                    slot,
                    epoch,
                    &format!("{stage}: {reason}"),
                    backoff,
                    config.schedule.retry_initial,
                );
                if let Some(slot) = slot.upgrade() {
                    slot.fail(epoch, reason.clone());
                }
                set_status(
                    slot,
                    epoch,
                    &config.status,
                    crate::context::SemanticIndexStatus::Failed(format!(
                        "semantic: unavailable: {reason}"
                    )),
                );
                crate::slog_warn!(
                    "semantic view unavailable root={} reason={}",
                    config.root.display(),
                    reason
                );
                #[cfg(test)]
                observe_fill(&config.root, "unavailable");
                if !wait_retry(slot, epoch, wake) {
                    return Err(RefreshError::Superseded);
                }
                backoff = (backoff * 2).min(config.schedule.retry_max);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Counts a running worker on its slot for as long as it lives.
struct WorkerGuard(Weak<CheckoutSemanticSlot>);

impl WorkerGuard {
    fn enter(slot: &Weak<CheckoutSemanticSlot>) -> Self {
        if let Some(slot) = slot.upgrade() {
            slot.workers.fetch_add(1, Ordering::SeqCst);
        }
        Self(slot.clone())
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.0.upgrade() {
            slot.workers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Runs a root's views-on semantic lane: starts the embedding model,
/// registers and loads the checkout, then fills it now and whenever the
/// fill scheduler decides to.
///
/// The worker holds the slot only weakly, so it stops when the root's
/// context is dropped or the lane is cleared.
pub(crate) fn run_worker(
    slot: Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: crossbeam_channel::Receiver<()>,
    config: WorkerConfig,
) {
    let _counted = WorkerGuard::enter(&slot);
    // A lane cleared or restarted before this worker began (two quick
    // configures) does nothing at all: no model, no registration.
    if !is_current(&slot, epoch) {
        return;
    }
    let fail = |reason: String| {
        clear_retry(&slot, epoch);
        crate::slog_warn!(
            "semantic view unavailable root={} reason={}",
            config.root.display(),
            reason
        );
        if let Some(slot) = slot.upgrade() {
            slot.fail(epoch, reason.clone());
        }
        set_status(
            &slot,
            epoch,
            &config.status,
            crate::context::SemanticIndexStatus::Failed(format!("semantic: unavailable: {reason}")),
        );
        #[cfg(test)]
        observe_fill(&config.root, "unavailable");
    };
    let mut model = None;
    let fingerprint =
        match retry_activation(&slot, epoch, &wake, &config, "embedding model", || {
            if model.is_none() {
                model = Some(crate::semantic_index::EmbeddingModel::from_config(
                    &config.semantic,
                )?);
            }
            model
                .as_mut()
                .unwrap()
                .fingerprint_for_view(&config.semantic)
        }) {
            Ok(fingerprint) => fingerprint,
            Err(RefreshError::Superseded) => return,
            Err(error) => return fail(format!("embedding model: {error}")),
        };
    let mut model = model.unwrap();
    let producer = SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps);
    if !is_current(&slot, epoch) {
        return;
    }
    let runtime = match CheckoutSemantic::new(
        &config.storage,
        &config.family,
        &config.scope,
        &config.root,
        producer,
        slot.clone(),
    ) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => return fail(format!("view registration: {error}")),
    };
    if !is_current(&slot, epoch) {
        return;
    }
    // Publications of this lane's view pass the lane fence, so a superseded
    // worker can no longer publish once this lane's epoch began.
    runtime
        .driver()
        .set_publish_fence(Arc::new(LaneFence::new(slot.clone(), epoch)));
    // Subscribe to watcher changes and write intents before the first
    // snapshot is served, so no edit made during the load is missed.
    config.drivers.install(Arc::clone(runtime.driver()));
    let started = Instant::now();
    #[cfg(any(test, feature = "test-timing-hooks"))]
    delay_startup_io_for_test("SEMANTIC_VIEW_LOAD");
    if let Err(error) =
        retry_activation(&slot, epoch, &wake, &config, "view load", || runtime.load())
    {
        config.drivers.clear_if(runtime.driver());
        if error != RefreshError::Superseded {
            fail(format!("view load: {error}"));
        }
        return;
    }
    // Build the index queries score before the lane is served, so neither
    // the first query nor a readiness sample has to build it.
    let _ = runtime.index();
    crate::slog_info!(
        "semantic view loaded root={} family={} scope={} load_ms={}",
        config.root.display(),
        config.family,
        config.scope,
        started.elapsed().as_millis()
    );
    let installed = slot
        .upgrade()
        .is_some_and(|slot| slot.install(epoch, Arc::clone(&runtime)));
    if !installed {
        config.drivers.clear_if(runtime.driver());
        return;
    }
    set_status(
        &slot,
        epoch,
        &config.status,
        crate::context::SemanticIndexStatus::ready(),
    );
    drop(runtime);
    serve_fills_with_status(
        &slot,
        epoch,
        &wake,
        &config.schedule,
        Some(&config.status),
        &mut |texts| model.embed(texts),
    );
}

/// What one catch-up left behind.
enum FillOutcome {
    /// Nothing more to do until the next wake.
    Settled,
    /// Work is left because of a transient error or a paused root; retry
    /// after a wait. `progress` is whether this attempt got anything done.
    Retry {
        progress: bool,
        reason: Option<String>,
    },
    /// The lane is no longer this worker's.
    Stop,
}

/// Simulate a slow storage read, not CPU contention. Child-process tests get
/// an observation file before the delay so requests can target this exact phase.
#[cfg(any(test, feature = "test-timing-hooks"))]
pub(crate) fn delay_startup_io_for_test(phase: &str) {
    if let Some(path) = std::env::var_os(format!("AFT_TEST_{phase}_START_FILE")) {
        std::fs::write(path, b"started").expect("startup I/O observation");
    }
    if let Some(ms) = std::env::var(format!("AFT_TEST_{phase}_DELAY_MS"))
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

#[cfg(test)]
static FILL_OBSERVERS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<PathBuf, crossbeam_channel::Sender<&'static str>>>,
> = std::sync::LazyLock::new(|| Mutex::new(Default::default()));

#[cfg(test)]
pub(crate) struct FillObserver {
    root: PathBuf,
    rx: crossbeam_channel::Receiver<&'static str>,
}
#[cfg(test)]
impl FillObserver {
    pub(crate) fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        lock(&FILL_OBSERVERS).insert(root.clone(), tx);
        Self { root, rx }
    }
    pub(crate) fn next(&self) -> &'static str {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("semantic fill event")
    }
}
#[cfg(test)]
impl Drop for FillObserver {
    fn drop(&mut self) {
        lock(&FILL_OBSERVERS).remove(&self.root);
    }
}
#[cfg(test)]
fn observe_fill(root: &Path, event: &'static str) {
    if let Some(tx) = lock(&FILL_OBSERVERS).get(root) {
        let _ = tx.send(event);
    }
}

/// Fills the lane's installed checkout now, then again after the quiet
/// window following each wake, and after a backoff when a fill left work
/// behind because of a transient error. Returns when the lane is cleared,
/// restarted or dropped.
#[cfg(test)]
pub(crate) fn serve_fills<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
    schedule: &FillSchedule,
    embed: &mut F,
) where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    serve_fills_with_status(slot, epoch, wake, schedule, None, embed);
}

fn serve_fills_with_status<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
    schedule: &FillSchedule,
    status: Option<&RwLock<crate::context::SemanticIndexStatus>>,
    embed: &mut F,
) where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    let budget = FillBudget {
        max_batch: schedule.max_batch.max(1),
        ..FillBudget::default()
    };
    let mut backoff = schedule.retry_initial;
    let mut retry_after = None;
    let mut first = true;
    loop {
        if !first {
            let woke = match retry_after {
                Some((_, true)) => {
                    if !wait_retry(slot, epoch, wake) {
                        return;
                    }
                    false
                }
                // Rebinding a paused checkout should resume it immediately;
                // only failed backend probes are held to a retry deadline.
                Some((wait, false)) => match wake.recv_timeout(wait) {
                    Ok(()) => true,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => false,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                },
                None => match wake.recv() {
                    Ok(()) => true,
                    Err(_) => return,
                },
            };
            #[cfg(test)]
            if woke {
                observe_fill(&schedule.root, "wake");
                if lock(&FILL_OBSERVERS).contains_key(&schedule.root) {
                    let delay = std::env::var("AFT_TEST_TIMING_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    std::thread::sleep(Duration::from_millis(delay));
                }
            }
            if woke && !wait_quiet(wake, schedule.quiet_window) {
                return;
            }
        }
        first = false;
        let outcome = if (schedule.paused)() {
            #[cfg(test)]
            observe_fill(&schedule.root, "paused");
            FillOutcome::Retry {
                progress: false,
                reason: None,
            }
        } else {
            match slot
                .upgrade()
                .filter(|slot| slot.is_current(epoch))
                .and_then(|slot| slot.runtime())
            {
                Some(runtime) => catch_up(slot, epoch, &runtime, schedule, budget, embed),
                None => FillOutcome::Stop,
            }
        };
        match outcome {
            FillOutcome::Stop => return,
            FillOutcome::Settled => {
                clear_retry(slot, epoch);
                if let Some(status) = status {
                    let value = match slot
                        .upgrade()
                        .and_then(|slot| slot.runtime())
                        .and_then(|runtime| runtime.unavailable_reason())
                    {
                        Some(reason) => crate::context::SemanticIndexStatus::Failed(format!(
                            "semantic: unavailable: {reason}"
                        )),
                        None => crate::context::SemanticIndexStatus::ready(),
                    };
                    set_status(slot, epoch, status, value);
                }
                #[cfg(test)]
                observe_fill(&schedule.root, "settled");
                backoff = schedule.retry_initial;
                retry_after = None;
            }
            FillOutcome::Retry { progress, reason } => {
                if progress {
                    backoff = schedule.retry_initial;
                }
                retry_after = Some((backoff, reason.is_some()));
                if let Some(reason) = reason {
                    let reason =
                        schedule_retry(slot, epoch, &reason, backoff, schedule.retry_initial);
                    if let Some(runtime) = slot.upgrade().and_then(|slot| slot.runtime()) {
                        runtime.set_unavailable(reason.clone());
                    }
                    if let Some(status) = status {
                        set_status(
                            slot,
                            epoch,
                            status,
                            crate::context::SemanticIndexStatus::Failed(format!(
                                "semantic: unavailable: {reason}"
                            )),
                        );
                    }
                    #[cfg(test)]
                    observe_fill(&schedule.root, "retry");
                }
                backoff = (backoff * 2).min(schedule.retry_max);
            }
        }
    }
}

/// Waits until wakes stop arriving for `quiet_window`. False when the lane's
/// wake channel closed.
fn wait_quiet(wake: &crossbeam_channel::Receiver<()>, quiet_window: Duration) -> bool {
    let mut deadline = Instant::now() + quiet_window;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };
        match wake.recv_timeout(remaining) {
            Ok(()) => deadline = Instant::now() + quiet_window,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => return true,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Fills `runtime` in budgeted rounds until no budgeted work remains or a
/// round makes no progress.
fn catch_up<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    runtime: &CheckoutSemantic,
    schedule: &FillSchedule,
    budget: FillBudget,
    embed: &mut F,
) -> FillOutcome
where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    let mut progress = false;
    let mut telemetry = None;
    loop {
        let Some(_permit) = crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
            &schedule.limiter,
            LIMITER_KIND,
            &schedule.root,
            || is_current(slot, epoch),
        ) else {
            return FillOutcome::Stop;
        };
        if telemetry.is_none() {
            telemetry = Some(crate::cold_build_limiter::progress::start(
                &schedule.root,
                LIMITER_KIND,
                None,
                crate::cold_build_limiter::progress::StartLog::Info,
            ));
            crate::cold_build_limiter::progress::phase("embedding", None);
        }
        let elapsed_started = Instant::now();
        let embedding_timing_scope = crate::semantic_index::EmbeddingTimingScope::new();
        let refresh = runtime.refresh(budget, embed);
        let embedding_timing = embedding_timing_scope.snapshot();
        drop(embedding_timing_scope);
        let elapsed_ms = elapsed_started.elapsed().as_millis();
        match refresh {
            Ok(report) => {
                if report.model_calls > 0
                    || report.installed > 0
                    || !report.errors.is_empty()
                    || !report.store_errors.is_empty()
                {
                    crate::slog_info!(
                        "semantic view fill root={} queued={} deferred={} embedded_keys={} chunks={} chunk_reused={} texts={} calls={} stored_hits={} resident_hits={} installed={} failed={} errors={} store_errors={} elapsed_ms={} embed_ms={} embed_max_ms={}",
                        schedule.root.display(),
                        report.queued,
                        report.deferred,
                        report.embedded_keys,
                        report.chunks,
                        report.chunk_reused,
                        report.embedded_texts,
                        report.model_calls,
                        report.stored_hits,
                        report.resident_hits,
                        report.installed,
                        report.failed,
                        report.errors.len(),
                        report.store_errors.len(),
                        elapsed_ms,
                        embedding_timing.total_ms(),
                        embedding_timing.max_ms()
                    );
                }
                let advanced = report.installed > 0 || report.failed > 0;
                progress |= advanced;
                // A store that refuses what was embedded will refuse it again
                // on a timer: record it as the checkout's unavailable reason
                // and wait for the next wake.
                if let Some(error) = report.store_errors.first() {
                    mark_unavailable(runtime, schedule, format!("semantic store: {error}"));
                    warm_index(runtime);
                    return FillOutcome::Settled;
                }
                if !report.errors.is_empty() {
                    if let Some(error) = report
                        .errors
                        .iter()
                        .find(|error| !crate::semantic_index::embedding_failure_is_transient(error))
                    {
                        mark_unavailable(runtime, schedule, error.clone());
                        warm_index(runtime);
                        return FillOutcome::Settled;
                    }
                    crate::slog_warn!(
                        "semantic view fill left work for a retry root={} error={}",
                        schedule.root.display(),
                        report.errors[0]
                    );
                    warm_index(runtime);
                    return FillOutcome::Retry {
                        progress,
                        reason: report.errors.first().cloned(),
                    };
                }
                runtime.clear_unavailable();
                // Work beyond the budget continues at once; a round that made
                // no progress without an error has nothing it can do now.
                if report.deferred == 0 || !advanced {
                    warm_index(runtime);
                    return FillOutcome::Settled;
                }
            }
            Err(RefreshError::Transient(error)) => {
                crate::slog_warn!(
                    "semantic view refresh will retry root={} error={}",
                    schedule.root.display(),
                    error
                );
                return FillOutcome::Retry {
                    progress,
                    reason: Some(error),
                };
            }
            Err(RefreshError::Unavailable(error)) => {
                mark_unavailable(runtime, schedule, error);
                return FillOutcome::Settled;
            }
            // A restart replaced this lane while it folded; its work is
            // dropped and the replacement owns the view.
            Err(RefreshError::Superseded) => return FillOutcome::Stop,
        }
    }
}

/// Records a refresh error that retrying on a timer would not fix as the
/// checkout's unavailable reason, logging it once. The next wake (an edit, a rebind, a reconfigure)
/// tries again, and a clean refresh clears it.
fn mark_unavailable(runtime: &CheckoutSemantic, schedule: &FillSchedule, reason: String) {
    if runtime.set_unavailable(reason.clone()) {
        crate::slog_warn!(
            "semantic view unavailable root={} reason={}",
            schedule.root.display(),
            reason
        );
    }
}

/// A failed refresh, by whether waiting can fix it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefreshError {
    /// Clears by itself: the checkout changed or was being written while it
    /// was loaded, or a SQLite database was busy or locked by another
    /// connection.
    Transient(String),
    /// Needs something to change first: an unreadable or corrupt store, a
    /// refused registration or publication, a manifest that does not match.
    Unavailable(String),
    /// The lane was replaced; the lane fence refused this publication.
    Superseded,
}

impl RefreshError {
    fn classify(reason: String) -> Self {
        if reason.contains(super::first_load::SUPERSEDED_PUBLISH) {
            Self::Superseded
        } else if is_transient_reason(&reason) {
            Self::Transient(reason)
        } else {
            Self::Unavailable(reason)
        }
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(reason) | Self::Unavailable(reason) => f.write_str(reason),
            Self::Superseded => f.write_str(super::first_load::SUPERSEDED_PUBLISH),
        }
    }
}

/// Whether a model, load or store failure clears by itself. Embedding errors
/// carry the backend's authoritative transient marker. The only store failure
/// retried is SQLite's busy/locked
/// (another connection holds the database for a moment); every other store
/// error is surfaced as the checkout's named gap.
pub fn is_transient_reason(reason: &str) -> bool {
    crate::semantic_index::embedding_failure_is_transient(reason)
        || [
            super::first_load::CHECKOUT_CHANGED_DURING_RECONCILE,
            super::first_load::CHECKOUT_WRITE_ACTIVE,
            super::first_load::CHECKOUT_CHANGED_BEFORE_INSTALL,
            // SQLITE_BUSY and SQLITE_LOCKED, as rusqlite renders them.
            "database is locked",
            "database table is locked",
            "database is busy",
        ]
        .iter()
        .any(|marker| reason.contains(marker))
}

/// Rebuilds the scored index after a fill, on the worker rather than on the
/// next query.
fn warm_index(runtime: &CheckoutSemantic) {
    let _ = runtime.index();
}

#[cfg(test)]
#[path = "semantic_runtime_tests.rs"]
mod tests;
