//! The data layer inside a running AFT, while it is built beside the stores
//! it replaces.
//!
//! It is on unless the process runs with `AFT_DATA_LAYER=0`, an undocumented
//! switch that lasts only while the cutover removes today's stores: tools
//! read from it where they have been wired to. When it is on, the first
//! query of a root binds the root's checkout and, if the checkout has no
//! publication yet, builds it on a thread of its own. AFT's own edits reach
//! it through the edit-intent listener, and outside changes through the
//! watcher's path batches.
//!
//! Text queries see a change at once. The call graph is refreshed lazily,
//! since agents edit far more often than they read it (about 14 edits per
//! call-graph read in mason sessions): once when a checkout is bound or
//! built, once [`QUIET`] after the last recorded change, and whenever a
//! reader that needs exact answers waits for it ([`DataCheckout::fresh_view`]).
//! A refresh resolves every change recorded since the last one in one pass,
//! so a burst of edits costs one update.
//!
//! Builds and refreshes run as the root's maintenance on its executor (see
//! [`Runner`]), so an unbound root does none and an unbind cancels them at
//! their checkpoints; a configure asks for them as it binds the root
//! ([`root_bound`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use aft_store::ids::{Digest128, FamilyId};

use super::checkout::{Checkout, CheckoutView, DataError, DataLayer, Planes};
use super::meaning::{Meaning, MeaningState, Round};
use crate::executor::{JobCancellation, Lane, RootMaintenanceExecutor};
use crate::write_intent::{WriteIntent, WriteIntentListener, WritePhase};

/// Whether this process runs the data layer.
#[must_use]
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if FORCED_ON.with(std::cell::Cell::get) {
        return true;
    }
    // On by default while the cutover removes today's stores; `AFT_DATA_LAYER=0`
    // runs today's route, so a test run can tell a new failure from an old one.
    std::env::var_os("AFT_DATA_LAYER").is_none_or(|value| value != "0")
}

#[cfg(test)]
thread_local! {
    static FORCED_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turns the data layer on for the calling thread, so a test can run tools
/// against it without setting the process-wide switch other tests read.
#[cfg(test)]
pub(crate) fn force_on_for_test() {
    FORCED_ON.with(|forced| forced.set(true));
}

/// The process's data layer, once the first checkout binds.
static LAYER: OnceLock<Arc<DataLayer>> = OnceLock::new();

/// The process's data layer: one block cache for every checkout's reads.
fn layer() -> Arc<DataLayer> {
    Arc::clone(LAYER.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        Arc::new(DataLayer::new(256 << 20, threads))
    }))
}

/// The family a checkout at `root` belongs to: its repository's root
/// commits, so every worktree and clone of one repository shares a store.
/// A folder Git confirms is not a repository is a family of its own.
///
/// # Errors
///
/// When Git could not tell. Keying such a folder by its path would give it
/// a store of its own that the repository's checkouts never share, so the
/// bind waits and tries again instead.
fn family_of(root: &Path) -> Result<FamilyId, DataError> {
    let identity = crate::search_index::root_commit_identity_or_folder(root)
        .map_err(DataError::Identity)?
        .unwrap_or_else(|| format!("folder:{}", root.display()));
    Ok(FamilyId::from_digest(Digest128::of(identity.as_bytes())))
}

/// How long a checkout goes without a recorded change before its call
/// graph is refreshed unasked. Edits a few seconds apart, as an agent makes
/// them, never refresh in between.
pub(crate) const QUIET: Duration = Duration::from_secs(120);

/// How long a root whose checkout could not be bound waits before the next
/// bind is tried, at first; each failure in a row doubles it, up to
/// [`BIND_RETRY_MAX`].
const BIND_RETRY: Duration = Duration::from_secs(5);
const BIND_RETRY_MAX: Duration = Duration::from_secs(300);

/// How long a failed build waits before it is tried again, at first; each
/// failure in a row doubles it, up to [`BUILD_RETRY_MAX`].
const BUILD_RETRY: Duration = Duration::from_secs(5);
const BUILD_RETRY_MAX: Duration = Duration::from_secs(300);

/// Roots whose last bind failed: when to try again, how many failed in a
/// row, and the error, so tool calls in between don't each reopen the store.
fn bind_failures() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, BindFailure>> {
    static FAILURES: OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, BindFailure>>> =
        OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

struct BindFailure {
    retry_at: Instant,
    in_a_row: u32,
    /// Kept whole, so a bind tried again before `retry_at` fails as the
    /// last one did, a refused store file included.
    error: DataError,
}

fn backoff(first: Duration, max: Duration, in_a_row: u32) -> Duration {
    first
        .saturating_mul(1u32 << in_a_row.saturating_sub(1).min(16))
        .min(max)
}

/// Clears a flag when dropped, so work that stops, by returning, by a
/// panic, or by being dropped unrun, never leaves itself marked as running.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a reader that needs exact call-graph answers gets.
pub(crate) enum Fresh {
    /// A view whose call graph covers every recorded change.
    Ready(CheckoutView),
    /// The call graph is being built or refreshed and did not finish in
    /// time.
    Building,
    /// No call graph can be served: the last build or refresh failed for
    /// this reason, and nothing is running now. A build is retried later.
    Failed(String),
}

/// When the changes since a publication are published on their own, after
/// a refresh resolves them: at once past this many changed paths, and
/// otherwise when the checkout goes quiet.
const PUBLISH_AT: usize = 64;

/// A checkout whose text and call graph read from more segments than this
/// is built again whole, which writes everything it reads into one segment
/// and retires what no other checkout reads; each publication of its
/// changes adds one. Vector segments are not counted, since a build does
/// not fold them.
const COMPACT_AT: usize = 8;

/// Whether a refresh publishes the changes it resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Publish {
    /// Only past [`PUBLISH_AT`] changed paths.
    WhenMany,
    /// Whenever any path changed: the checkout went quiet.
    Always,
}

/// Executor request ids of a checkout's background work.
const BUILD_REQUEST: &str = "data-layer-build";
const REFRESH_REQUEST: &str = "data-layer-refresh";
const RESCAN_REQUEST: &str = "data-layer-rescan";
const FILL_REQUEST: &str = "data-layer-meaning-fill";

/// Where a checkout's background work runs. Under the daemon it runs as the
/// root's maintenance on its executor: the executor runs none while the root
/// is unbound, and a root that unbinds cancels it at its checkpoints. A
/// standalone AFT has no executor and serves one root that is always bound,
/// so there the work runs on a thread of its own.
struct Runner {
    root: PathBuf,
    executor: parking_lot::Mutex<Option<RootMaintenanceExecutor>>,
}

impl Runner {
    /// Runs `work` on `lane`. Work the executor refuses (the root is
    /// unbound, or the executor shut down) is dropped unrun, so anything it
    /// captured to mark itself running is released with it. Never falls back
    /// to a thread under the daemon: that would put a retired root to work.
    fn run(&self, request_id: &'static str, lane: Lane, work: impl FnOnce() + Send + 'static) {
        let executor = self.executor.lock().clone();
        let Some(executor) = executor else {
            if let Err(error) = std::thread::Builder::new()
                .name(format!("aft-{request_id}"))
                .spawn(work)
            {
                crate::slog_warn!(
                    "data layer could not start {request_id} for {}: {error}",
                    self.root.display()
                );
            }
            return;
        };
        let cancellation = JobCancellation::new().with_root(&self.root);
        let token = cancellation.clone();
        executor.submit_maintenance(
            request_id,
            lane,
            cancellation,
            Box::new(move |_| {
                let _cancel = crate::executor::current_job_cancellation()
                    .is_none()
                    .then(|| crate::executor::install_job_cancellation(token));
                work();
                crate::protocol::Response::success(request_id, serde_json::Value::Null)
            }),
        );
    }
}

/// When a failed build is tried again.
#[derive(Default)]
struct BuildRetry {
    /// No build starts before this; a waiting thread starts one then.
    at: Option<Instant>,
    in_a_row: u32,
}

/// A checkout and the state of its background work, shared by the work
/// itself and the handle that starts it.
struct Work {
    checkout: Arc<Checkout>,
    runner: Runner,
    /// A build is queued or running.
    building: Arc<AtomicBool>,
    /// A call-graph refresh is queued or running.
    refreshing: Arc<AtomicBool>,
    /// A rescan is queued or running.
    rescanning: Arc<AtomicBool>,
    retry: std::sync::Mutex<BuildRetry>,
    /// Told when a build, refresh or fill round ends (see
    /// [`DataCheckout::on_status_change`]).
    status_listener: std::sync::Mutex<Option<StatusListener>>,
    /// Meaning search: its vector plane and the fill that embeds for it.
    meaning: Meaning,
    /// A fill round is queued or running.
    filling: Arc<AtomicBool>,
    /// When a failed fill is tried again.
    fill_retry: std::sync::Mutex<BuildRetry>,
}

impl Work {
    /// Starts a build unless one is queued or running, or a failed one is
    /// waiting to be tried again. A build that fails is tried again after a
    /// wait that doubles each time, until one succeeds or the checkout is
    /// dropped; the wait is a sleeping thread, never an executor worker.
    fn start_build(self: &Arc<Self>) {
        {
            let retry = self
                .retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if retry.at.is_some_and(|at| Instant::now() < at) {
                return;
            }
        }
        if self.building.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = ClearOnDrop(Arc::clone(&self.building));
        let work = Arc::clone(self);
        self.runner.run(BUILD_REQUEST, Lane::HeavyInit, move || {
            let result = work.checkout.build();
            drop(flag);
            if !matches!(result, Err(DataError::Cancelled)) {
                work.notify_status_change();
            }
            match result {
                Ok(report) => {
                    *work
                        .retry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = BuildRetry::default();
                    crate::slog_info!(
                        "data layer built {}: {} files, {} parsed, {} bytes",
                        work.checkout.root().display(),
                        report.files,
                        report.parsed_files,
                        report.segment_bytes
                    );
                    // Changes recorded while the build ran stay laid over its
                    // publication and wait for the call graph.
                    work.start_refresh(Publish::WhenMany);
                    // Meaning search fills what the published files need.
                    work.start_fill();
                }
                // Runs again when the root is bound and asks for it.
                Err(DataError::Cancelled) => {}
                Err(error) => work.retry_build_later(&error),
            }
        });
    }

    fn retry_build_later(self: &Arc<Self>, error: &DataError) {
        let wait = {
            let mut retry = self
                .retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            retry.in_a_row += 1;
            let wait = backoff(BUILD_RETRY, BUILD_RETRY_MAX, retry.in_a_row);
            retry.at = Some(Instant::now() + wait);
            wait
        };
        crate::slog_warn!(
            "data layer build failed for {} (trying again in {} s): {error}",
            self.checkout.root().display(),
            wait.as_secs()
        );
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("aft-data-build-retry".to_string())
            .spawn(move || {
                std::thread::sleep(wait);
                if let Some(work) = weak.upgrade() {
                    work.start_build();
                }
            });
    }

    /// Resolves the call graph again until no recorded change is pending,
    /// unless a refresh is queued or running already. A change recorded
    /// while it runs is picked up by its next pass, or, if it was just
    /// ending, by the check after it lets go of the flag. A refresh that
    /// finds the stored call graph damaged starts a build, which replaces it.
    fn start_refresh(self: &Arc<Self>, publish: Publish) {
        if !self.checkout.planes().callgraph {
            // Without a call graph there is nothing to resolve: the changes
            // laid over the text index are folded in by a text-only build,
            // which takes about a second, once the checkout goes quiet or
            // many have piled up.
            let changed = self.checkout.changed_paths();
            if changed > 0 && (publish == Publish::Always || changed >= PUBLISH_AT) {
                self.start_build();
            }
            return;
        }
        let unpublished = publish == Publish::Always && self.checkout.changed_paths() > 0;
        if !(pending(&self.checkout) || unpublished) || self.refreshing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let flag = ClearOnDrop(Arc::clone(&self.refreshing));
        let work = Arc::clone(self);
        // A refresh changes only the checkout, behind its own lock, so it
        // runs beside the root's reads, one at a time per root, without
        // taking a heavy build's slot.
        self.runner
            .run(REFRESH_REQUEST, Lane::MaintenanceCommit, move || {
                let mut flag = Some(flag);
                loop {
                    let result = work.checkout.refresh_callgraph();
                    if !matches!(result, Ok(None) | Err(DataError::Cancelled)) {
                        work.notify_status_change();
                    }
                    if matches!(result, Ok(Some(_))) {
                        continue;
                    }
                    match &result {
                        Err(DataError::Cancelled) => return,
                        Err(error) => crate::slog_warn!(
                            "data layer call-graph refresh failed for {}: {error}",
                            work.checkout.root().display()
                        ),
                        Ok(_) => work.maybe_publish(publish),
                    }
                    drop(flag.take());
                    match result {
                        Err(DataError::Damaged(_)) => {
                            work.start_build();
                            return;
                        }
                        // The changes stay pending; the next change or query
                        // that waits for the call graph tries again.
                        Err(_) => return,
                        Ok(_) => {}
                    }
                    if !pending(&work.checkout) || work.refreshing.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    flag = Some(ClearOnDrop(Arc::clone(&work.refreshing)));
                }
            });
    }

    /// Publishes the changes a refresh resolved, when `publish` asks for it,
    /// and builds the store again whole once publications of changes have
    /// piled up.
    fn maybe_publish(self: &Arc<Self>, publish: Publish) {
        let changed = self.checkout.changed_paths();
        if changed == 0 || (publish == Publish::WhenMany && changed < PUBLISH_AT) {
            return;
        }
        match self.checkout.publish_changes() {
            Ok(Some(report)) => {
                crate::slog_info!(
                    "data layer published changes for {}: {} paths, {} fragments, {} text units, {} segments, {:.0} ms",
                    self.checkout.root().display(),
                    report.paths,
                    report.fragments,
                    report.text_units,
                    report.segments,
                    report.ms
                );
                if report.segments > COMPACT_AT {
                    self.start_build();
                }
            }
            Ok(None) | Err(DataError::Cancelled) => {}
            Err(error) => crate::slog_warn!(
                "data layer could not publish changes for {}: {error}",
                self.checkout.root().display()
            ),
        }
    }

    /// Builds the checkout if it needs it, and refreshes its call graph
    /// otherwise.
    fn catch_up(self: &Arc<Self>, publish: Publish) {
        if self.checkout.needs_build() {
            // Nothing published, or only the text index: a build that
            // published it first stopped before the call graph.
            self.start_build();
        } else {
            self.start_refresh(publish);
        }
    }

    fn running(&self) -> bool {
        self.building.load(Ordering::Acquire)
            || self.refreshing.load(Ordering::Acquire)
            || self.rescanning.load(Ordering::Acquire)
    }

    /// Compares the checkout's files with its state, records what changed
    /// unreported, then catches up as a bind does. The checkout reports its
    /// call graph building from now until the rescan ends, so a query made
    /// right after the bind waits for it.
    fn start_rescan(self: &Arc<Self>) {
        if self.rescanning.swap(true, Ordering::AcqRel) {
            return;
        }
        self.checkout.begin_rescan();
        // Ends the rescan even when the executor refuses the job (the root
        // unbound before it ran), so the call graph is not left building.
        struct Ended(Arc<Work>);
        impl Drop for Ended {
            fn drop(&mut self) {
                self.0.checkout.end_rescan();
                self.0.rescanning.store(false, Ordering::Release);
            }
        }
        let ended = Ended(Arc::clone(self));
        self.runner
            .run(RESCAN_REQUEST, Lane::MaintenanceCommit, move || {
                let work = Arc::clone(&ended.0);
                if let Err(error) = work.checkout.rescan() {
                    if !matches!(error, DataError::Cancelled) {
                        crate::slog_warn!(
                            "data layer could not compare {} with its files: {error}",
                            work.checkout.root().display()
                        );
                    }
                }
                drop(ended);
                work.catch_up(Publish::WhenMany);
            });
    }

    /// Starts a round of meaning search's fill, unless semantic search is
    /// off, a round is queued or running, or a failed fill is waiting to be
    /// tried again. A round that leaves files starts the next one, so a fill
    /// runs as a chain of jobs and other work on the root runs between
    /// them. A failed round is tried again after a wait that doubles each
    /// time, as a failed build is.
    fn start_fill(self: &Arc<Self>) {
        if !self.meaning.enabled() {
            return;
        }
        {
            let retry = self
                .fill_retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if retry.at.is_some_and(|at| Instant::now() < at) {
                return;
            }
        }
        if self.filling.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = ClearOnDrop(Arc::clone(&self.filling));
        let work = Arc::clone(self);
        self.runner.run(FILL_REQUEST, Lane::HeavyInit, move || {
            if crate::executor::current_job_cancelled() {
                // Runs again when the root is bound and a search asks.
                return;
            }
            let round = work.meaning.round(&work.checkout);
            drop(flag);
            work.notify_status_change();
            match round {
                Round::More => work.start_fill(),
                Round::Done => {
                    *work
                        .fill_retry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = BuildRetry::default();
                    let progress = work.meaning.progress().unwrap_or_default();
                    crate::slog_info!(
                        "data layer meaning fill finished for {}: {} files, {} chunks embedded",
                        work.checkout.root().display(),
                        progress.files_done,
                        progress.chunks_embedded
                    );
                }
                Round::Idle => {}
                Round::Failed(error) => work.retry_fill_later(&error),
            }
        });
    }

    fn retry_fill_later(self: &Arc<Self>, error: &str) {
        let wait = {
            let mut retry = self
                .fill_retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            retry.in_a_row += 1;
            let wait = backoff(BUILD_RETRY, BUILD_RETRY_MAX, retry.in_a_row);
            retry.at = Some(Instant::now() + wait);
            wait
        };
        crate::slog_warn!(
            "data layer meaning fill failed for {} (trying again in {} s): {error}",
            self.checkout.root().display(),
            wait.as_secs()
        );
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("aft-data-fill-retry".to_string())
            .spawn(move || {
                std::thread::sleep(wait);
                if let Some(work) = weak.upgrade() {
                    work.start_fill();
                }
            });
    }

    /// Tells the status listener, if any, that the checkout's status may
    /// have changed.
    fn notify_status_change(&self) {
        let listener = self
            .status_listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(listener) = listener {
            listener();
        }
    }
}

/// One root's checkout in a running AFT.
pub(crate) struct DataCheckout {
    work: Arc<Work>,
    quiet: Arc<QuietTimer>,
    /// Set once a bind has used the scan that opening the checkout made;
    /// every later bind compares the files again (see [`root_bound`]).
    bound_once: AtomicBool,
    /// Kept so the edit-intent listener lives as long as the checkout.
    _listener: Arc<dyn WriteIntentListener>,
}

impl Drop for DataCheckout {
    // An evicted checkout must not leave its quiet-timer thread asleep for
    // up to `QUIET`: its refresh would find nothing to refresh anyway.
    fn drop(&mut self) {
        self.quiet.stop();
    }
}

impl DataCheckout {
    /// [`Self::open_planes`] with every plane.
    #[cfg(test)]
    pub(crate) fn open(
        root: &Path,
        storage: &Path,
        executor: Option<RootMaintenanceExecutor>,
    ) -> Result<Arc<Self>, DataError> {
        Self::open_planes(root, storage, executor, Planes::ALL)
    }

    /// Binds the checkout at `root`, with its family store under
    /// `storage`, and builds or refreshes `planes` in the background as it
    /// needs. Under the daemon, `executor` is the root's: the background work
    /// runs as its maintenance. `None` runs it on threads, for a standalone
    /// AFT.
    ///
    /// # Errors
    ///
    /// When Git could not tell the repository's identity, or the family
    /// store cannot be opened.
    pub(crate) fn open_planes(
        root: &Path,
        storage: &Path,
        executor: Option<RootMaintenanceExecutor>,
        planes: Planes,
    ) -> Result<Arc<Self>, DataError> {
        {
            let failures = bind_failures()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(failure) = failures.get(root) {
                if Instant::now() < failure.retry_at {
                    return Err(failure.error.clone());
                }
            }
        }
        let bound = family_of(root).and_then(|family| {
            let family_dir = storage.join("data").join(family.digest().to_string());
            Checkout::bind_planes(layer(), &family_dir, family, root, planes)
        });
        let checkout = match bound {
            Ok(checkout) => {
                bind_failures()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(root);
                Arc::new(checkout)
            }
            Err(error) => {
                let mut failures = bind_failures()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let in_a_row = failures.get(root).map_or(0, |failure| failure.in_a_row) + 1;
                failures.insert(
                    root.to_path_buf(),
                    BindFailure {
                        retry_at: Instant::now() + backoff(BIND_RETRY, BIND_RETRY_MAX, in_a_row),
                        in_a_row,
                        error: error.clone(),
                    },
                );
                return Err(error);
            }
        };
        let work = Arc::new(Work {
            runner: Runner {
                root: checkout.root().to_path_buf(),
                executor: parking_lot::Mutex::new(executor),
            },
            checkout,
            building: Arc::new(AtomicBool::new(false)),
            refreshing: Arc::new(AtomicBool::new(false)),
            rescanning: Arc::new(AtomicBool::new(false)),
            retry: std::sync::Mutex::default(),
            status_listener: std::sync::Mutex::new(None),
            meaning: Meaning::default(),
            filling: Arc::new(AtomicBool::new(false)),
            fill_retry: std::sync::Mutex::default(),
        });
        let quiet = {
            let work = Arc::downgrade(&work);
            Arc::new(QuietTimer::new(QUIET, move || {
                if let Some(work) = work.upgrade() {
                    work.start_refresh(Publish::Always);
                    // Edited files wait for vectors of their new content.
                    work.start_fill();
                }
            }))
        };
        let listener: Arc<dyn WriteIntentListener> = Arc::new(EditListener {
            checkout: Arc::downgrade(&work.checkout),
            root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            quiet: Arc::clone(&quiet),
        });
        crate::write_intent::register_listener(&listener);
        // A bind may have recorded what changed while nothing watched.
        work.catch_up(Publish::WhenMany);
        Ok(Arc::new(Self {
            work,
            quiet,
            bound_once: AtomicBool::new(false),
            _listener: listener,
        }))
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        self.work.checkout.root()
    }

    #[must_use]
    pub(crate) fn view(&self) -> Option<CheckoutView> {
        self.work.checkout.view()
    }

    #[must_use]
    pub(crate) fn checkout(&self) -> &Checkout {
        &self.work.checkout
    }

    /// Runs the checkout's background work on `executor` from now on: the
    /// root's executor installed after the checkout was bound.
    pub(crate) fn set_executor(&self, executor: RootMaintenanceExecutor) {
        *self.work.runner.executor.lock() = Some(executor);
    }

    /// Compares the checkout's files with its state and records what
    /// changed, for a watcher that lost events (see [`Work::start_rescan`]).
    pub(crate) fn rescan(&self) {
        self.work.start_rescan();
    }

    /// Records the watcher's changed `paths` (absolute).
    pub(crate) fn note_paths(&self, paths: &[PathBuf]) {
        let root = std::fs::canonicalize(self.root()).unwrap_or_else(|_| self.root().to_path_buf());
        record(&self.work.checkout, &root, paths);
        self.quiet.changed();
    }

    /// Builds the checkout if it needs it, and otherwise makes sure a
    /// call-graph refresh runs if any change is pending: when the root is
    /// bound, and for a query about to wait for the call graph.
    pub(crate) fn request_refresh(&self) {
        self.work.catch_up(Publish::WhenMany);
    }

    /// A view whose call graph covers every recorded change, for a reader
    /// that needs exact answers: refreshes now and waits for it, up to
    /// `wait`. Reports a failure at once when the last build or refresh
    /// failed and nothing is running to replace it.
    pub(crate) fn fresh_view(&self, wait: Duration) -> Fresh {
        self.request_refresh();
        let deadline = Instant::now() + wait;
        loop {
            if let Some(view) = self
                .work
                .checkout
                .view()
                .filter(|view| view.status().callgraph == super::checkout::PlaneStatus::Ready)
            {
                return Fresh::Ready(view);
            }
            if !self.work.running() {
                if let Some(failure) = self.work.checkout.failure() {
                    return Fresh::Failed(failure);
                }
            }
            if Instant::now() >= deadline {
                return Fresh::Building;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Calls `fire` once a checkout has gone `quiet` without a recorded change
/// (for the call graph, [`QUIET`], to refresh it). One timer thread runs per
/// checkout at most, and only while a change is waiting: each change moves
/// the deadline on, and the thread sleeps until the latest one.
struct QuietTimer {
    quiet: Duration,
    /// When the last change was recorded, in milliseconds since `epoch`.
    last_change_ms: AtomicU64,
    armed: AtomicBool,
    epoch: Instant,
    fire: Box<dyn Fn() + Send + Sync>,
    /// Set when the checkout is dropped: the waiting thread ends at once
    /// instead of sleeping out its deadline.
    stopped: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}

impl QuietTimer {
    fn new(quiet: Duration, fire: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            quiet,
            last_change_ms: AtomicU64::new(0),
            armed: AtomicBool::new(false),
            epoch: Instant::now(),
            fire: Box::new(fire),
            stopped: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        }
    }

    /// Ends the waiting thread, if any, without firing; later changes start
    /// none.
    fn stop(&self) {
        *self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.wake.notify_all();
    }

    /// Sleeps up to `duration`, or until [`Self::stop`]; true once stopped.
    fn sleep(&self, duration: Duration) -> bool {
        let stopped = self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (stopped, _) = self
            .wake
            .wait_timeout_while(stopped, duration, |stopped| !*stopped)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *stopped
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Notes a recorded change, and starts the timer thread unless one is
    /// waiting already.
    fn changed(self: &Arc<Self>) {
        self.last_change_ms.store(self.now_ms(), Ordering::Release);
        if self.sleep(Duration::ZERO) || self.armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let timer = Arc::clone(self);
        let quiet_ms = u64::try_from(self.quiet.as_millis()).unwrap_or(u64::MAX);
        let spawned = std::thread::Builder::new()
            .name("aft-data-quiet".to_string())
            .spawn(move || loop {
                let due = timer
                    .last_change_ms
                    .load(Ordering::Acquire)
                    .saturating_add(quiet_ms);
                let now = timer.now_ms();
                if now < due {
                    if timer.sleep(Duration::from_millis(due - now)) {
                        break;
                    }
                    continue;
                }
                timer.armed.store(false, Ordering::Release);
                // A change recorded between the deadline check and letting
                // go of the flag found the timer armed and started none; if
                // there was one, this thread keeps waiting for it.
                if timer
                    .last_change_ms
                    .load(Ordering::Acquire)
                    .saturating_add(quiet_ms)
                    > timer.now_ms()
                    && !timer.armed.swap(true, Ordering::AcqRel)
                {
                    continue;
                }
                (timer.fire)();
                break;
            });
        if spawned.is_err() {
            self.armed.store(false, Ordering::Release);
        }
    }
}

/// Whether recorded changes wait for a published call graph. Before one is
/// published there is nothing to refresh: the build that makes it covers
/// every change recorded before its scan.
fn pending(checkout: &Checkout) -> bool {
    // Pending changes, not the plane's word: a rescan in progress also
    // reports the call graph building, with nothing yet for a refresh.
    let status = checkout.status();
    status.callgraph_published && status.callgraph_pending > 0
}

/// Called as a configure of `ctx`'s root succeeds, before its reply: when the
/// root's checkout was bound before, starts its rescan now, so the call graph
/// reports building before the reply and a query sent right after it waits
/// for what the rescan finds. The rescan job owns that state and ends it
/// however it ends, even unrun; [`root_bound`] in the configure tail starts
/// it again only if none is running. Binds nothing.
pub(crate) fn rebind_configured(ctx: &crate::context::AppContext) {
    use crate::data_layer::surfaces::DataCheckoutProbe;
    if let Some(DataCheckoutProbe::Bound(checkout)) = ctx.probe_data_checkout() {
        if checkout.bound_once.load(Ordering::Acquire) {
            checkout.work.start_rescan();
        }
    }
}

/// Has a root's checkout catch up as a configure binds the root: binds it on
/// the root's first bind, then builds it or refreshes its call graph, and
/// fills meaning search's vectors, as its root's maintenance. A configure
/// tail calls it, so the work starts when the root is bound rather than at
/// the first query.
///
/// Opening the checkout compared its files with what was published. A later
/// bind of the same checkout compares them again: files can change while the
/// root is unbound and nothing watches it, or just before a configure whose
/// watcher event has not arrived yet.
pub(crate) fn root_bound(ctx: &crate::context::AppContext) {
    if let Some(checkout) = ctx.data_checkout() {
        if checkout.bound_once.swap(true, Ordering::AcqRel) {
            checkout.work.start_rescan();
        } else {
            checkout.request_refresh();
        }
        checkout.configure_meaning(ctx.data_meaning_config().as_ref());
    }
}

/// Records changed absolute `paths` under `root`. The checkout scans each
/// one again, a folder with everything under it, under the same rules as a
/// build, so this only turns the paths into checkout-relative form.
fn record(checkout: &Arc<Checkout>, root: &Path, paths: &[PathBuf]) {
    let mut relative_paths = Vec::with_capacity(paths.len());
    for path in paths {
        let canonical_parent = path
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|| path.clone());
        let Ok(relative) = canonical_parent.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str().map(|text| text.replace('\\', "/")) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        relative_paths.push(relative.into_bytes());
    }
    if let Err(error) = checkout.record_paths(&relative_paths) {
        crate::slog_warn!(
            "data layer could not record changes under {}: {error}",
            checkout.root().display()
        );
    }
}

/// Hears AFT's own edits.
struct EditListener {
    checkout: Weak<Checkout>,
    root: PathBuf,
    quiet: Arc<QuietTimer>,
}

impl WriteIntentListener for EditListener {
    fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
        // After the write, the bytes are final (formatting and rollback
        // included).
        if phase != WritePhase::After {
            return;
        }
        let Some(checkout) = self.checkout.upgrade() else {
            return;
        };
        match change {
            WriteIntent::Paths(paths) => record(&checkout, &self.root, paths),
            // A folder is scanned again whole, under the scan's rules.
            WriteIntent::Directory(path) => {
                record(&checkout, &self.root, std::slice::from_ref(path))
            }
        }
        self.quiet.changed();
    }
}

/// The shared block cache's counters and its measured bytes in memory
/// (`None` while another thread holds its lock), or `None` while no checkout
/// of this process has bound (asking never creates the data layer).
#[must_use]
pub(crate) fn block_cache_memory() -> Option<(aft_store::cache::CacheStats, Option<usize>)> {
    LAYER
        .get()
        .map(|layer| (layer.cache_stats(), layer.cache_memory_bytes()))
}

/// Binds the checkout at `root`, with its family store under `storage`,
/// without the background builds, refreshes and edit listener a running AFT
/// adds: for a command-line build that publishes in the foreground and
/// exits.
///
/// # Errors
///
/// When Git could not tell the repository's identity, or the family store
/// cannot be opened.
pub(crate) fn bind_foreground(
    root: &Path,
    storage: &Path,
    planes: Planes,
) -> Result<Checkout, DataError> {
    let family = family_of(root)?;
    let family_dir = storage.join("data").join(family.digest().to_string());
    Checkout::bind_planes(layer(), &family_dir, family, root, planes)
}

/// The last failed build or call-graph refresh of a checkout.
pub(crate) type DataFailure = super::checkout::Failure;

impl DataCheckout {
    /// This checkout's last failed build or refresh, until one succeeds.
    #[must_use]
    pub(crate) fn last_failure(&self) -> Option<DataFailure> {
        self.work.checkout.last_failure()
    }

    /// Records an outcome as a build or refresh of this checkout would, for
    /// a test of what status reports for it.
    #[cfg(test)]
    pub(crate) fn note_outcome_for_test<T>(
        &self,
        stage: &'static str,
        result: &Result<T, DataError>,
    ) {
        self.work.checkout.note_outcome(stage, result);
    }

    /// Calls `notify` whenever a build, call-graph refresh or meaning fill
    /// round of this checkout ends, so status pushes follow the data layer.
    /// Replaces any earlier listener.
    pub(crate) fn on_status_change(&self, notify: StatusListener) {
        *self
            .work
            .status_listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(notify);
    }
}

/// What to call when a checkout's status may have changed.
pub(crate) type StatusListener = Arc<dyn Fn() + Send + Sync>;

impl DataCheckout {
    /// Gives meaning search the root's semantic configuration, `None` when
    /// semantic search is off or not admitted for it, as a configure binds
    /// the root; a changed one is filled under from now on. Starts a fill
    /// when files wait for vectors.
    pub(crate) fn configure_meaning(&self, config: Option<&crate::config::SemanticBackendConfig>) {
        self.work.meaning.configure(config);
        self.meaning(|| None);
    }

    /// How meaning search stands for the current view. `config` works out
    /// the root's semantic configuration if no configure has given one yet.
    /// Starts a fill when files wait for vectors and none is running, or
    /// none has learned the model yet. `None` before anything is published.
    pub(crate) fn meaning(
        &self,
        config: impl FnOnce() -> Option<crate::config::SemanticBackendConfig>,
    ) -> Option<(CheckoutView, MeaningState)> {
        self.work.meaning.configure_once(config);
        let view = self.view()?;
        let state = self.work.meaning.state(&view);
        let wanted = match &state {
            MeaningState::Off => false,
            MeaningState::Starting | MeaningState::Failed(_) => true,
            MeaningState::Serving { snapshot, .. } => super::meaning::waiting(snapshot.coverage()),
        };
        if wanted {
            self.work.start_fill();
        }
        Some((view, state))
    }

    /// How meaning search stands for the current view, for status reads:
    /// starts nothing. `None` before anything is published.
    pub(crate) fn meaning_status(&self) -> Option<(CheckoutView, MeaningState)> {
        let view = self.view()?;
        let state = self.work.meaning.state(&view);
        Some((view, state))
    }

    /// Whether a fill round is queued or running.
    pub(crate) fn filling(&self) -> bool {
        self.work.filling.load(Ordering::Acquire)
    }

    /// What the last fill round reported, if a fill ran.
    pub(crate) fn meaning_progress(&self) -> Option<super::vectors::FillProgress> {
        self.work.meaning.progress()
    }

    /// Gives meaning search `config` as [`Self::configure_meaning`] does,
    /// without starting a fill, so a test can read how the plane reports
    /// before any model loads.
    #[cfg(test)]
    pub(crate) fn configure_meaning_without_fill_for_test(
        &self,
        config: Option<&crate::config::SemanticBackendConfig>,
    ) {
        self.work.meaning.configure(config);
    }

    /// What meaning search holds in memory for this checkout; see
    /// [`super::meaning::Meaning::memory`].
    pub(crate) fn meaning_memory(&self, seen: &mut aft_store::heap::Seen) -> Option<usize> {
        self.work.meaning.memory(seen)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A process published a checkout's text index and stopped before its
    /// call graph. The next process to bind it builds the call graph, and
    /// its refresh thread does not spin while it waits.
    #[test]
    fn a_text_only_publication_is_built_on_the_next_bind() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "mod util;\npub fn caller() { util::callee(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("src/util.rs"), "pub fn callee() {}\n").unwrap();
        let family = family_of(&root).unwrap();
        let family_dir = storage
            .path()
            .join("data")
            .join(family.digest().to_string());
        {
            let first = Checkout::bind(layer(), &family_dir, family, &root).unwrap();
            assert!(first.publish_text_first().unwrap());
            assert!(first.needs_build());
            // Nothing to refresh before a call graph is published.
            assert!(!pending(&first));
        }
        let handle = DataCheckout::open(&root, storage.path(), None).unwrap();
        let view = match handle.fresh_view(Duration::from_secs(60)) {
            Fresh::Ready(view) => view,
            Fresh::Building => panic!("the call graph was never built"),
            Fresh::Failed(reason) => panic!("the build failed: {reason}"),
        };
        assert!(view.status().callgraph_published);
        assert!(!handle.checkout().needs_build());
    }

    /// Under the daemon a checkout's build is its root's maintenance: while
    /// the root is unbound the executor runs none of it, and once the root
    /// is bound again, the request its configure tail makes builds it.
    #[test]
    fn an_unbound_root_builds_nothing_until_it_is_bound() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "mod util;\npub fn caller() { util::callee(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("src/util.rs"), "pub fn callee() {}\n").unwrap();

        let ctx = Arc::new(crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ));
        ctx.mark_subc_bound();
        let executor = crate::executor::Executor::new();
        let root_id = crate::path_identity::ProjectRootId::from_path(&root).unwrap();
        assert!(executor.register_actor(root_id, Arc::clone(&ctx)));
        let maintenance = ctx
            .callgraph_executor_for_test()
            .expect("registering the actor installs its executor");
        ctx.mark_subc_unbound();

        let handle = DataCheckout::open(&root, storage.path(), Some(maintenance)).unwrap();
        // The executor refuses the build the bind asked for, and drops it
        // unrun: nothing is published, and nothing stays marked as running.
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.work.running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!handle.work.running(), "refused work stayed marked running");
        std::thread::sleep(Duration::from_millis(500));
        assert!(handle.view().is_none(), "an unbound root was built");
        assert!(handle.checkout().needs_build());

        ctx.mark_subc_bound();
        handle.request_refresh();
        let view = match handle.fresh_view(Duration::from_secs(60)) {
            Fresh::Ready(view) => view,
            Fresh::Building => panic!("the call graph was never built"),
            Fresh::Failed(reason) => panic!("the build failed: {reason}"),
        };
        assert!(view.status().callgraph_published);
    }

    #[test]
    fn the_quiet_timer_fires_once_after_a_burst_of_changes() {
        let fired = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&fired);
        let timer = Arc::new(QuietTimer::new(Duration::from_millis(200), move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        // Changes closer together than the quiet window never fire in
        // between.
        for _ in 0..5 {
            timer.changed();
            std::thread::sleep(Duration::from_millis(60));
        }
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // The next change arms it again.
        timer.changed();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fired.load(Ordering::SeqCst), 2);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(fired.load(Ordering::SeqCst), 2, "fired without a change");
    }

    #[test]
    fn a_stopped_quiet_timer_ends_its_thread_without_firing() {
        let fired = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&fired);
        let timer = Arc::new(QuietTimer::new(Duration::from_secs(120), move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        timer.changed();
        // The waiting thread holds the timer.
        assert_eq!(Arc::strong_count(&timer), 2);
        timer.stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&timer) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&timer), 1, "the thread is still waiting");
        // Changes after the stop start no thread.
        timer.changed();
        assert_eq!(Arc::strong_count(&timer), 1);
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }
}
