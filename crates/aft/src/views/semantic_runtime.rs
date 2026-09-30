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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Producer recorded for a plane this runtime does not register.
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
        Ok(Self {
            root: root.to_path_buf(),
            access: ViewAccess::Owner(owner.clone()),
            owner,
            driver,
            loader,
            plane,
            refresh: Mutex::new(()),
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
    /// The checkout is reloaded first when the installed snapshot carries
    /// edits it has not reconciled (a watcher change or an AFT write records
    /// an intent), so the fill sees the current bytes. Completions the fill
    /// installs are then folded into a newly published generation, which
    /// keeps them across restarts and lets sibling checkouts seed from them.
    pub fn refresh<F>(&self, budget: FillBudget, embed: &mut F) -> Result<FillReport, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        let _serial = lock(&self.refresh);
        let mut snapshot = self.installed();
        if needs_reload(&snapshot) {
            snapshot = self.load()?;
        }
        let driver = Arc::clone(&self.driver);
        let report = self
            .plane
            .fill(&self.owner, &snapshot, budget, embed, &move || {
                driver.installed_snapshot()
            })
            .map_err(|error| error.to_string())?;
        if report.installed > 0 || report.failed > 0 {
            self.load()?;
        }
        Ok(report)
    }

    /// Scores `query_vector` against the installed snapshot.
    pub fn search(
        &self,
        query_vector: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<SemanticQuery, String> {
        self.plane
            .search(
                &self.access,
                &self.root,
                &self.installed(),
                query_vector,
                top_k,
                include,
            )
            .map_err(|error| error.to_string())
    }

    /// The index `search` scores, for callers that need to hold it.
    pub fn index(&self) -> Result<Arc<SemanticIndex>, String> {
        self.plane
            .overlay(&self.access, &self.root, &self.installed())
            .map(|overlay| overlay.index)
            .map_err(|error| error.to_string())
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
        self.plane.unbind(&self.access);
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
}

impl CheckoutSemanticSlot {
    /// Starts a new epoch in the `Loading` state and returns it with the
    /// receiver the new worker waits on.
    pub fn begin(&self) -> (u64, crossbeam_channel::Receiver<()>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        *lock(&self.wake) = Some(sender);
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
        self.epoch.fetch_add(1, Ordering::SeqCst);
        *lock(&self.wake) = None;
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
}

/// Everything the fill worker needs besides the slot.
pub(crate) struct WorkerConfig {
    pub root: PathBuf,
    pub storage: PathBuf,
    pub family: String,
    pub scope: String,
    pub semantic: crate::config::SemanticBackendConfig,
    pub quiet_window: Duration,
    /// The root's semantic status, which every status and readiness surface
    /// reads. The worker keeps it in step with the lane.
    pub status: Arc<RwLock<crate::context::SemanticIndexStatus>>,
    /// The root's installed checkout driver, which receives watcher changes.
    pub drivers: InstalledDriver,
    pub limiter: Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    /// True while the root has been unbound past the abandon grace window;
    /// refreshes wait for a rebind instead of loading the embedder.
    pub paused: Box<dyn Fn() -> bool + Send>,
}

const LIMITER_KIND: &str = "semantic view fill";

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

/// Runs a root's views-on semantic lane: starts the embedding model,
/// registers and loads the checkout, then fills it now and after every
/// quiet window that follows a wake.
///
/// The worker holds the slot only weakly, so it stops when the root's
/// context is dropped or the lane is cleared.
pub(crate) fn run_worker(
    slot: Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: crossbeam_channel::Receiver<()>,
    config: WorkerConfig,
) {
    let fail = |reason: String| {
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
    };
    let mut model = match crate::semantic_index::EmbeddingModel::from_config(&config.semantic) {
        Ok(model) => model,
        Err(error) => return fail(format!("embedding model: {error}")),
    };
    let fingerprint = match model.fingerprint(&config.semantic) {
        Ok(fingerprint) => fingerprint,
        Err(error) => return fail(format!("embedding model: {error}")),
    };
    let producer = SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps);
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
    if !slot.upgrade().is_some_and(|slot| slot.is_current(epoch)) {
        return;
    }
    // Subscribe to watcher changes and write intents before the first
    // snapshot is served, so no edit made during the load is missed.
    config.drivers.install(Arc::clone(runtime.driver()));
    let started = Instant::now();
    if let Err(error) = runtime.load() {
        config.drivers.clear_if(runtime.driver());
        return fail(format!("view load: {error}"));
    }
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

    let budget = FillBudget {
        max_batch: config.semantic.max_batch_size.max(1),
        ..FillBudget::default()
    };
    let mut embed = |texts: Vec<String>| model.embed(texts);
    let current = || slot.upgrade().is_some_and(|slot| slot.is_current(epoch));
    let mut fill_until_done = |runtime: &CheckoutSemantic| loop {
        let Some(_permit) = crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
            &config.limiter,
            LIMITER_KIND,
            &config.root,
            current,
        ) else {
            return;
        };
        match runtime.refresh(budget, &mut embed) {
            Ok(report) => {
                if report.model_calls > 0 || report.installed > 0 || !report.errors.is_empty() {
                    crate::slog_info!(
                        "semantic view fill root={} queued={} deferred={} embedded_keys={} texts={} calls={} stored_hits={} resident_hits={} installed={} failed={} errors={}",
                        config.root.display(),
                        report.queued,
                        report.deferred,
                        report.embedded_keys,
                        report.embedded_texts,
                        report.model_calls,
                        report.stored_hits,
                        report.resident_hits,
                        report.installed,
                        report.failed,
                        report.errors.len()
                    );
                }
                // Work beyond the budget continues at once; a fill that made
                // no progress waits for the next wake instead of spinning.
                if report.deferred == 0 || (report.installed == 0 && report.failed == 0) {
                    return;
                }
            }
            Err(error) => {
                crate::slog_warn!(
                    "semantic view refresh failed root={} error={}",
                    config.root.display(),
                    error
                );
                return;
            }
        }
    };

    fill_until_done(&runtime);
    drop(runtime);
    while wake.recv().is_ok() {
        let mut deadline = Instant::now() + config.quiet_window;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match wake.recv_timeout(remaining) {
                Ok(()) => deadline = Instant::now() + config.quiet_window,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
        if (config.paused)() {
            continue;
        }
        let Some(runtime) = slot
            .upgrade()
            .filter(|slot| slot.is_current(epoch))
            .and_then(|slot| slot.runtime())
        else {
            return;
        };
        fill_until_done(&runtime);
    }
}

#[cfg(test)]
#[path = "semantic_runtime_tests.rs"]
mod tests;
