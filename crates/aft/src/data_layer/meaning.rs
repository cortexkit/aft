//! Meaning search inside a running AFT: a checkout's vector plane
//! ([`super::vectors`]), filled in the background with the configured
//! embedding model.
//!
//! It runs only with the data layer on (`AFT_DATA_LAYER=1`) and semantic
//! search switched on and admitted for the root. A fill embeds what the
//! checkout's current files need and no checkout of the repository has
//! embedded, one round of files at a time; the runtime runs each round as
//! one job of the root's maintenance, so other work on the root runs between
//! rounds. A fill starts when the checkout is built, when it goes quiet
//! after changes, and when a search finds files still waiting for vectors.
//!
//! Meaning search is ready only once every file it applies to within
//! `semantic.max_files` has its vectors or a recorded reason it has none.
//! Before that it serves the files filled so far, and the search says its
//! answer is partial; while nothing is filled it reports itself building.
//!
//! A round that fails is told apart as today's semantic build tells its
//! failures apart ([`FillFailure`]), so aft_search words each as it does
//! today: an embedding backend that cannot be reached for now, ONNX Runtime
//! still being downloaded, or a failure waiting will not fix.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::checkout::{Checkout, CheckoutView, DataError};
use super::old_index::OldIndex;
use super::vectors::{Coverage, FillBudget, FillProgress, MeaningSnapshot, Producer, VectorPlane};
use crate::config::SemanticBackendConfig;
use crate::semantic_index::SemanticEmbeddingModel;

/// Files whose vectors one fill job publishes together: a round of the
/// fill, and the step in which searches see its progress.
const FILES_PER_ROUND: usize = 256;

/// The stage a meaning plane reports before its first fill round has
/// learned the model's identity, which needs the model loaded (and, for a
/// remote backend, asked for its vector length).
pub(crate) const STARTING_STAGE: &str = "loading_embedding_model";

/// The stage a meaning plane reports while it fills and has nothing to
/// serve yet.
pub(crate) const FILLING_STAGE: &str = "filling";

/// The stage a meaning plane reports while its fill waits for another fill
/// of the same repository, in this process or another, to publish what it
/// is embedding; it then embeds only what that one did not.
pub(crate) const WAITING_STAGE: &str = "waiting_for_another_fill";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Why the last fill round stopped, told apart as today's semantic build
/// tells its failures apart. Each is retried; only the wording differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FillFailure {
    /// The embedding backend could not be reached, or refused for a while
    /// (a paused model lane, a transient error); its health records why.
    Unreachable(String),
    /// ONNX Runtime is missing while the plugin is still downloading it.
    WaitingForRuntime,
    /// Anything else, with the message today's index status would carry.
    Failed(String),
}

impl FillFailure {
    /// The message status and replies carry.
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Unreachable(reason) | Self::Failed(reason) => reason,
            Self::WaitingForRuntime => "waiting for the ONNX Runtime download to finish",
        }
    }
}

impl aft_store::heap::HeapBytes for FillFailure {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        match self {
            Self::Unreachable(reason) | Self::Failed(reason) => reason.heap_bytes(seen),
            Self::WaitingForRuntime => 0,
        }
    }
}

/// How a checkout's meaning search stands.
#[derive(Debug, Clone)]
pub(crate) enum MeaningState {
    /// Semantic search is off or not admitted for the root.
    Off,
    /// The checkout has published nothing yet, so there is nothing to fill.
    Unpublished,
    /// No fill has learned the model's identity yet, so nothing can be
    /// searched: the first round is queued or loading the model.
    Starting,
    /// The last fill failed before any plane could be read, with why.
    Blocked(FillFailure),
    /// The plane at the generation it read last, paired with the view.
    Serving {
        snapshot: MeaningSnapshot,
        /// The last fill's failure, kept until a fill succeeds: the files it
        /// did not reach stay pending.
        failure: Option<FillFailure>,
    },
}

/// What one fill round left to do.
#[derive(Debug)]
pub(crate) enum Round {
    /// Files remain for the next round.
    More,
    /// Every file the round set out to fill is filled.
    Done,
    /// Semantic search is off, or the checkout has nothing published yet.
    Idle,
    /// The round failed; the fill is tried again later.
    Failed(FillFailure),
}

/// The configuration a plane was made for, and the plane.
struct Configured {
    config: SemanticBackendConfig,
    plane: Arc<VectorPlane>,
}

/// One checkout's meaning search: its configuration, its plane once a fill
/// has made it, and the fill's last outcome.
#[derive(Default)]
pub(crate) struct Meaning {
    /// AFT's storage folder, where the plugin downloads ONNX Runtime.
    storage: PathBuf,
    /// The root's semantic configuration as the last configure (or, before
    /// any, the first search) gave it: `Some(None)` while semantic search
    /// is off or not admitted for the root, `None` before anything gave one.
    config: Mutex<Option<Option<SemanticBackendConfig>>>,
    /// The plane for `config`, made by the first fill round under it.
    plane: Mutex<Option<Configured>>,
    /// The embedding model a fill uses, with the configuration it was made
    /// for, kept between the fill's rounds and dropped once the fill
    /// finishes, so an idle checkout holds no model. Only a fill round
    /// takes it, for the whole round.
    model: Mutex<Option<(SemanticBackendConfig, SemanticEmbeddingModel)>>,
    /// The last fill failure, until a fill round succeeds.
    failure: Mutex<Option<FillFailure>>,
    /// The last round's progress through its fill.
    progress: Mutex<Option<FillProgress>>,
    /// When the fill under way started and when a fill last stored what
    /// it filled, for health (see [`FillTimes`]).
    times: Mutex<FillTimes>,
    /// What the fill under way, or the last one, did over its rounds.
    totals: Mutex<FillTotals>,
    /// Set for a checkout a parent folder's session holds for one of its
    /// child repositories ([`Self::read_only`]): its plane only reads the
    /// vectors the repository's store already holds, and no fill runs for
    /// it, since semantic search is not admitted for a repository no session
    /// has opened.
    reads_only: std::sync::atomic::AtomicBool,
    /// When a read-only plane last looked for a newer generation.
    refreshed_at: Mutex<Option<std::time::Instant>>,
    /// A fill under the current configuration has finished, so files still
    /// waiting for vectors are being refreshed after edits, not embedded for
    /// the first time: status reports the plane ready with a refreshing
    /// count, as today's index does. Cleared when a new configuration makes
    /// a new plane, whose first fill is a build again. Kept in memory only:
    /// after a restart the plane counts as built once a fill finishes.
    filled: std::sync::atomic::AtomicBool,
}

/// How long a read-only plane serves the generation it read before it looks
/// for a newer one: looking costs a directory listing and a file read.
const READ_ONLY_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// When meaning search's fill moved, for health's stuck-plane check. Both
/// in milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FillTimes {
    /// When the first round of the fill under way started; `None` when no
    /// fill is under way. A fill lasts from its first round until a round
    /// finishes it or finds nothing to do; a failed round keeps it open, so
    /// a fill that keeps failing keeps its start.
    pub(crate) started_at_ms: Option<u64>,
    /// When a round last stored vectors and chunk maps
    /// ([`FillProgress::stored_at_ms`]), in this fill or an earlier one.
    pub(crate) stored_at_ms: Option<u64>,
}

/// What one fill did over all its rounds, for the line the log gets when
/// it finishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FillTotals {
    /// Files whose chunk maps it stored.
    pub(crate) files: usize,
    /// Chunks whose vectors it took from the old meaning index.
    pub(crate) chunks_imported: usize,
    /// Chunks it sent to the embedding model.
    pub(crate) chunks_embedded: usize,
}

impl Meaning {
    /// Meaning search for a checkout of a store under `storage`, AFT's
    /// storage folder.
    pub(crate) fn new(storage: &Path) -> Self {
        Self {
            storage: storage.to_path_buf(),
            ..Self::default()
        }
    }

    /// Takes `config` as the root's semantic configuration (`None` when
    /// semantic search is off for it). A different configuration drops the
    /// plane, so the next fill round loads the new model and embeds with it.
    /// Returns whether it changed.
    pub(crate) fn configure(&self, config: Option<&SemanticBackendConfig>) -> bool {
        let mut current = lock(&self.config);
        if current
            .as_ref()
            .is_some_and(|current| current.as_ref() == config)
        {
            return false;
        }
        *current = Some(config.cloned());
        drop(current);
        // A round in progress keeps its model; the next round sees the
        // configuration changed and loads the new one.
        *lock(&self.plane) = None;
        *lock(&self.failure) = None;
        *lock(&self.progress) = None;
        *lock(&self.times) = FillTimes::default();
        *lock(&self.totals) = FillTotals::default();
        *lock(&self.refreshed_at) = None;
        true
    }

    /// Makes this checkout's meaning search read only the vectors its
    /// repository's store holds for `config`'s model, whose identity is
    /// `fingerprint`, whichever checkout embedded them: no fill ever runs
    /// for it, and files without vectors stay unfilled. Reads the newest
    /// generation now if the last read is over [`READ_ONLY_REFRESH`] old.
    ///
    /// # Errors
    ///
    /// When the plane cannot be made.
    pub(crate) fn read_only(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
        fingerprint: &crate::semantic_index::SemanticIndexFingerprint,
    ) -> Result<(), String> {
        self.reads_only
            .store(true, std::sync::atomic::Ordering::Release);
        self.configure(Some(config));
        self.plane_for(checkout, config, || Ok(fingerprint.clone()))?;
        self.refresh_read_only();
        Ok(())
    }

    /// Whether this checkout's meaning search only reads (see
    /// [`Self::read_only`]).
    pub(crate) fn reads_only(&self) -> bool {
        self.reads_only.load(std::sync::atomic::Ordering::Acquire)
    }

    /// For a read-only plane, reads the store's newest generation if the
    /// last read is over [`READ_ONLY_REFRESH`] old; does nothing otherwise.
    pub(crate) fn refresh_read_only(&self) {
        if !self.reads_only() {
            return;
        }
        let Some(plane) = lock(&self.plane)
            .as_ref()
            .map(|configured| Arc::clone(&configured.plane))
        else {
            return;
        };
        {
            let mut refreshed_at = lock(&self.refreshed_at);
            if refreshed_at.is_some_and(|at| at.elapsed() < READ_ONLY_REFRESH) {
                return;
            }
            *refreshed_at = Some(std::time::Instant::now());
        }
        *lock(&self.failure) = plane
            .refresh()
            .err()
            .map(|error| FillFailure::Failed(error.to_string()));
    }

    /// Takes `config()` as the root's semantic configuration unless one was
    /// given already: working it out reads the filesystem, so searches ask
    /// once and each configure gives it again ([`Self::configure`]).
    pub(crate) fn configure_once(&self, config: impl FnOnce() -> Option<SemanticBackendConfig>) {
        if lock(&self.config).is_none() {
            self.configure(config().as_ref());
        }
    }

    /// Whether semantic search is on for the root.
    pub(crate) fn enabled(&self) -> bool {
        matches!(&*lock(&self.config), Some(Some(_)))
    }

    fn config(&self) -> Option<SemanticBackendConfig> {
        lock(&self.config).clone().flatten()
    }

    pub(super) fn synapse_lane(&self) -> Option<String> {
        self.config()
            .filter(|config| config.backend == crate::config::SemanticBackend::Synapse)
            .map(|config| config.model)
    }

    /// How meaning search stands for `view`, the checkout's current view
    /// (`None` before anything is published).
    pub(crate) fn state(&self, view: Option<&CheckoutView>) -> MeaningState {
        self.state_inner(view, false)
    }

    pub(crate) fn cached_state(&self, view: Option<&CheckoutView>) -> MeaningState {
        self.state_inner(view, true)
    }

    #[cfg(test)]
    pub(crate) fn refresh_for_test(&self, view: Option<&CheckoutView>) {
        *lock(&self.refreshed_at) = None;
        self.refresh_read_only();
        self.state(view);
    }

    fn state_inner(&self, view: Option<&CheckoutView>, cached_only: bool) -> MeaningState {
        if !self.enabled() {
            return MeaningState::Off;
        }
        let Some(view) = view else {
            return MeaningState::Unpublished;
        };
        let plane = lock(&self.plane)
            .as_ref()
            .map(|configured| Arc::clone(&configured.plane));
        let failure = lock(&self.failure).clone();
        let Some(plane) = plane else {
            return match failure {
                Some(failure) => MeaningState::Blocked(failure),
                None => MeaningState::Starting,
            };
        };
        let snapshot = if cached_only {
            match plane.cached_snapshot(view) {
                Some(snapshot) => Ok(snapshot),
                // No counts are certified for this view yet; never label an
                // older view's coverage as current during refresh.
                None => return MeaningState::Starting,
            }
        } else {
            plane.snapshot(view)
        };
        match snapshot {
            Ok(snapshot) => MeaningState::Serving { snapshot, failure },
            Err(error) => MeaningState::Blocked(FillFailure::Failed(error.to_string())),
        }
    }

    /// The last fill round's progress, if a fill ran.
    pub(crate) fn progress(&self) -> Option<FillProgress> {
        *lock(&self.progress)
    }

    /// When the fill under way started and when a fill last stored.
    pub(crate) fn times(&self) -> FillTimes {
        *lock(&self.times)
    }

    /// What the fill under way, or the last one, did over its rounds.
    pub(crate) fn totals(&self) -> FillTotals {
        *lock(&self.totals)
    }

    /// Records that a round starts: the fill's start, unless a fill is
    /// already under way.
    pub(crate) fn note_round_started(&self) {
        let mut times = lock(&self.times);
        if times.started_at_ms.is_none() {
            times.started_at_ms = Some(super::vectors::unix_ms());
            *lock(&self.totals) = FillTotals::default();
        }
    }

    /// Records how a round ended: one that finished the fill, or found
    /// nothing to do, ends it.
    pub(crate) fn note_round_ended(&self, outcome: &Round) {
        if matches!(outcome, Round::Done | Round::Idle) {
            lock(&self.times).started_at_ms = None;
        }
        if matches!(outcome, Round::Done) {
            self.filled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Whether a fill under the current configuration has finished; see
    /// the `filled` field.
    pub(crate) fn filled(&self) -> bool {
        self.filled.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Records a round's progress report, and when it stored, if it did.
    pub(crate) fn note_progress(&self, step: FillProgress) {
        *lock(&self.progress) = Some(step);
        if let Some(stored) = step.stored_at_ms {
            lock(&self.times).stored_at_ms = Some(stored);
        }
    }

    /// What meaning search holds in memory for the checkout, measured as the
    /// [`aft_store::heap`] module describes: its plane (see
    /// [`VectorPlane::memory`]) and the last fill failure's text. `None`
    /// while another thread holds the plane's slot; nothing here waits. The
    /// embedding model a fill keeps between its rounds is not counted: a
    /// local model is shared by the process and counted once with its
    /// embedding sessions, and a remote one holds no model in this process.
    pub(crate) fn memory(&self, seen: &mut aft_store::heap::Seen) -> Option<usize> {
        use aft_store::heap::HeapBytes as _;
        let plane = match self.plane.try_lock() {
            Ok(slot) => slot
                .as_ref()
                .map(|configured| Arc::clone(&configured.plane)),
            Err(std::sync::TryLockError::Poisoned(slot)) => slot
                .into_inner()
                .as_ref()
                .map(|configured| Arc::clone(&configured.plane)),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let plane = plane.map_or(0, |plane| {
            if seen.first(Arc::as_ptr(&plane)) {
                std::mem::size_of::<VectorPlane>() + plane.memory(seen)
            } else {
                0
            }
        });
        Some(plane + self.failure.heap_bytes(seen))
    }

    /// Runs one round of the fill for `checkout`: loads the model if no
    /// round of this fill has, makes the plane if no round under this
    /// configuration has, and fills one round of files.
    pub(crate) fn round(&self, checkout: &Arc<Checkout>) -> Round {
        let _alloc_label = crate::alloc_trace::scope(
            crate::alloc_trace::Label::MEANING_FILL,
            Some(checkout.root()),
        );
        let Some(config) = self.config() else {
            return Round::Idle;
        };
        if self.reads_only() {
            return Round::Idle;
        }
        if checkout.view().is_none() {
            // The build that publishes the checkout starts the next fill.
            return Round::Idle;
        }
        self.note_round_started();
        let mut model = lock(&self.model);
        let outcome = self.fill_round(checkout, &config, &mut model);
        match &outcome {
            Round::More => {
                *lock(&self.failure) = None;
                crate::semantic_index::clear_embedding_backend_retry_status(checkout.root());
            }
            Round::Done | Round::Idle => {
                *model = None;
                *lock(&self.failure) = None;
                crate::semantic_index::clear_embedding_backend_retry_status(checkout.root());
            }
            Round::Failed(failure) => {
                // A failed embedder may recover; load it again next time.
                *model = None;
                *lock(&self.failure) = Some(failure.clone());
            }
        }
        self.note_round_ended(&outcome);
        outcome
    }

    /// Tells `error` apart as today's semantic build does, and records an
    /// unreachable backend in its health, which aft_search and status read
    /// for the root at `root`. `loading` is true for an error loading the
    /// model, which is where a missing ONNX Runtime shows.
    fn failure(&self, root: &Path, error: String, loading: bool) -> FillFailure {
        if loading
            && error.starts_with(crate::semantic_index::ONNX_RUNTIME_MISSING_PREFIX)
            && matches!(
                crate::semantic_index::late_onnx_runtime(&self.storage),
                crate::semantic_index::LateOnnxRuntime::Downloading
            )
        {
            return FillFailure::WaitingForRuntime;
        }
        let clean = crate::semantic_index::strip_transient_embedding_marker(&error);
        // A paused model lane is recorded by the client that met it.
        if crate::synapse_embed::LanePause::from_error(&error).is_some() {
            return FillFailure::Unreachable(clean);
        }
        if crate::semantic_index::embedding_failure_is_transient(&error) {
            crate::semantic_index::note_embedding_backend_build_failure(root, &error);
            return FillFailure::Unreachable(clean);
        }
        FillFailure::Failed(clean)
    }

    fn fill_round(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
        model: &mut Option<(SemanticBackendConfig, SemanticEmbeddingModel)>,
    ) -> Round {
        if model
            .as_ref()
            .is_none_or(|(made_for, _)| made_for != config)
        {
            *model = None;
            #[cfg(test)]
            while lock(model_holds_for_test()).contains(checkout.root()) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            #[cfg(test)]
            if let Some(error) = model_failure_for_test(checkout.root()) {
                return Round::Failed(self.failure(checkout.root(), error, true));
            }
            #[cfg(test)]
            if let Some(dimensions) = model_free_dimensions_for_test(checkout.root()) {
                return self.fill_without_model_for_test(checkout, config, dimensions);
            }
            match SemanticEmbeddingModel::from_config(config) {
                Ok(loaded) => *model = Some((config.clone(), loaded)),
                Err(error) => return Round::Failed(self.failure(checkout.root(), error, true)),
            }
        }
        let (_, model) = model.as_mut().expect("loaded above");
        let plane = match self.plane_for(checkout, config, || model.fingerprint(config)) {
            Ok(plane) => plane,
            Err(error) => return Round::Failed(self.failure(checkout.root(), error, false)),
        };
        let budget = FillBudget {
            max_batch: model.max_batch_size(),
            files_per_round: FILES_PER_ROUND,
        };
        let filled = plane.fill_rounds(&mut |texts| model.embed(texts), budget, 1, &mut |step| {
            self.note_progress(step);
        });
        if let Ok(report) = &filled {
            let mut totals = lock(&self.totals);
            totals.files += report.files_filled;
            totals.chunks_imported += report.chunks_imported;
            totals.chunks_embedded += report.chunks_embedded;
        }
        match filled {
            Err(DataError::Cancelled) => Round::Idle,
            Err(error) => Round::Failed(FillFailure::Failed(error.to_string())),
            Ok(report) => match report.error {
                Some(error) => Round::Failed(self.failure(checkout.root(), error, false)),
                None if report.files_left > 0 => Round::More,
                None => Round::Done,
            },
        }
    }

    /// The plane for `config`, made now if no round under it has: its
    /// producer is the model's `fingerprint`, as today's index records it.
    fn plane_for(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
        fingerprint: impl FnOnce() -> Result<crate::semantic_index::SemanticIndexFingerprint, String>,
    ) -> Result<Arc<VectorPlane>, String> {
        let mut slot = lock(&self.plane);
        if let Some(configured) = slot
            .as_ref()
            .filter(|configured| &configured.config == config)
        {
            return Ok(Arc::clone(&configured.plane));
        }
        let fingerprint = fingerprint()?;
        // A read-only plane never fills, so it never opens the old index.
        let old = OldIndex::of_checkout(&self.storage, checkout.root(), fingerprint.clone());
        let plane = Arc::new(
            VectorPlane::new(
                Arc::clone(checkout),
                Producer::from_fingerprint(&fingerprint),
                config.max_files,
            )
            .reusing_old_index(old),
        );
        *slot = Some(Configured {
            config: config.clone(),
            plane: Arc::clone(&plane),
        });
        self.filled
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(plane)
    }
}

/// A fill round with no model, for a test whose checkout holds no file
/// meaning search applies to: the plane is made for `dimensions` and the
/// round finds nothing to embed.
#[cfg(test)]
impl Meaning {
    fn fill_without_model_for_test(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
        dimensions: usize,
    ) -> Round {
        let plane = match self.plane_for(checkout, config, || {
            Ok(
                crate::semantic_index::SemanticIndexFingerprint::for_config_dimension(
                    config, dimensions,
                ),
            )
        }) {
            Ok(plane) => plane,
            Err(error) => return Round::Failed(FillFailure::Failed(error)),
        };
        let budget = FillBudget {
            max_batch: 64,
            files_per_round: FILES_PER_ROUND,
        };
        match plane.fill_rounds(
            &mut |_| Err("this test fills with no model".to_string()),
            budget,
            1,
            &mut |_| {},
        ) {
            Ok(report) if report.error.is_none() && report.files_left == 0 => Round::Done,
            Ok(report) => Round::Failed(FillFailure::Failed(format!("{report:?}"))),
            Err(error) => Round::Failed(FillFailure::Failed(error.to_string())),
        }
    }
}

#[cfg(test)]
fn model_free_planes_for_test() -> &'static Mutex<std::collections::HashMap<PathBuf, usize>> {
    static PLANES: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, usize>>> =
        std::sync::OnceLock::new();
    PLANES.get_or_init(Default::default)
}

#[cfg(test)]
fn model_free_dimensions_for_test(root: &Path) -> Option<usize> {
    lock(model_free_planes_for_test()).get(root).copied()
}

/// Makes fills under `root` make their plane for `dimensions` without
/// loading the model, until the returned guard drops: for a test whose
/// embedding backend serves one session only, which its query must have.
#[cfg(test)]
pub(crate) fn fill_without_model_for_test(root: &Path, dimensions: usize) -> impl Drop {
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            lock(model_free_planes_for_test()).remove(&self.0);
        }
    }
    lock(model_free_planes_for_test()).insert(root.to_path_buf(), dimensions);
    Release(root.to_path_buf())
}

#[cfg(test)]
fn model_holds_for_test() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static HOLDS: std::sync::OnceLock<Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    HOLDS.get_or_init(Default::default)
}

/// Holds every model load of a fill under `root` until the returned guard
/// drops, so a test can look at a plane that is being read back.
#[cfg(test)]
pub(crate) fn hold_model_loads_for_test(root: &Path) -> impl Drop {
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            lock(model_holds_for_test()).remove(&self.0);
        }
    }
    lock(model_holds_for_test()).insert(root.to_path_buf());
    Release(root.to_path_buf())
}

/// Errors the next model load under a root fails with, for tests of how a
/// failed fill is reported (a missing ONNX Runtime cannot be staged on
/// every machine).
#[cfg(test)]
fn model_failures_for_test() -> &'static Mutex<std::collections::HashMap<PathBuf, String>> {
    static FAILURES: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, String>>> =
        std::sync::OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

#[cfg(test)]
fn model_failure_for_test(root: &Path) -> Option<String> {
    lock(model_failures_for_test()).get(root).cloned()
}

/// Makes every model load of a fill under `root` fail with `error`, until
/// the returned guard drops.
#[cfg(test)]
pub(crate) fn fail_model_loads_for_test(root: &Path, error: &str) -> impl Drop {
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            lock(model_failures_for_test()).remove(&self.0);
        }
    }
    lock(model_failures_for_test()).insert(root.to_path_buf(), error.to_string());
    Release(root.to_path_buf())
}

/// Whether `coverage` leaves files waiting for vectors that a fill would
/// embed.
#[must_use]
pub(crate) fn waiting(coverage: &Coverage) -> bool {
    !coverage.pending.is_empty()
}
