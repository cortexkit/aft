//! The data layer inside a running AFT.
//!
//! The first query of a root binds the root's checkout and, if the checkout
//! has no publication yet, builds it on a thread of its own. AFT's own edits reach
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

use super::checkout::{Checkout, CheckoutView, DataError, DataLayer, DeltaReport, Folded, Planes};
use super::meaning::{Meaning, MeaningState, Round};
use crate::executor::{JobCancellation, Lane, RootMaintenanceExecutor};
use crate::write_intent::{WriteIntent, WriteIntentListener, WritePhase};

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
    let identity = crate::root_identity::key::root_commit_identity_or_folder(root)
        .map_err(DataError::Identity)?
        .unwrap_or_else(|| format!("folder:{}", root.display()));
    Ok(FamilyId::from_digest(Digest128::of(identity.as_bytes())))
}

/// How long a checkout goes without a recorded change before its call
/// graph is refreshed unasked. Edits a few seconds apart, as an agent makes
/// them, never refresh in between.
pub(crate) const QUIET: Duration = Duration::from_secs(120);

/// [`QUIET`], or in a debug build the period `AFT_TEST_DATA_QUIET_MS` names,
/// so an end-to-end test that runs the binary and edits a file need not wait
/// two minutes for the edit to be refreshed. Release builds ignore it.
fn quiet_period() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("AFT_TEST_DATA_QUIET_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    QUIET
}

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

/// How many changed paths warrant a publication after edits settle,
/// rather than waiting for the longer quiet period.
const PUBLISH_AT: usize = 64;

/// Also schedule a publication once the call graph has resolved this many
/// files again since the publication. An edit to a widely used type re-resolves
/// hundreds of files whose callers' method calls it reaches, and the engine
/// holds each resolution, about half a megabyte in a large repository, until
/// the changes are published.
const PUBLISH_AT_REFRESHED: usize = 256;

/// Let the next edit's fresh-view request finish before starting a potentially
/// expensive publication. Each recorded change restarts this window.
const PUBLISH_SETTLE: Duration = Duration::from_secs(3);

/// Bound unpublished memory even when an agent keeps editing without a
/// settle window, measured from when a publication threshold was first met.
const PUBLISH_MAX_WAIT: Duration = Duration::from_secs(30);

/// Bound resolution memory immediately when it grows too quickly to wait
/// for either the settle window or the time cap.
const PUBLISH_MAX_REFRESHED: usize = 4 * PUBLISH_AT_REFRESHED;

fn settle_period() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("AFT_TEST_DATA_SETTLE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    PUBLISH_SETTLE
}

/// Whether a refresh publishes the changes it resolved.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Publish {
    /// Defer past either publication threshold, unless a memory cap is met.
    WhenMany,
    /// Publish past either threshold once the recorded changes have settled.
    Settled,
    /// Whenever any path changed: the checkout went quiet.
    Always,
}

/// Executor request ids of a checkout's background work.
const BUILD_REQUEST: &str = "data-layer-build";
const FOLD_REQUEST: &str = "data-layer-fold";
const REFRESH_REQUEST: &str = "data-layer-refresh";
const RESCAN_REQUEST: &str = "data-layer-rescan";
const FILL_REQUEST: &str = "data-layer-meaning-fill";

/// The process-wide pool a kind of background work runs in. Each pool runs
/// at most `cap` jobs at once, across every checkout, and queues the rest in
/// submission order.
#[derive(Clone, Copy, Debug)]
enum Pool {
    /// Builds: CPU- and memory-heavy, so few at once.
    Build,
    /// Meaning-search fill rounds, which mostly wait on an embedding backend.
    Fill,
    /// Call-graph refreshes and rescans after edits: short, and wanted soon.
    Refresh,
}

impl Pool {
    fn cap(self) -> usize {
        match self {
            Pool::Build => 2,
            Pool::Fill => 2,
            Pool::Refresh => 4,
        }
    }

    fn state(self) -> &'static parking_lot::Mutex<PoolState> {
        static BUILD: parking_lot::Mutex<PoolState> = parking_lot::Mutex::new(PoolState::new());
        static FILL: parking_lot::Mutex<PoolState> = parking_lot::Mutex::new(PoolState::new());
        static REFRESH: parking_lot::Mutex<PoolState> = parking_lot::Mutex::new(PoolState::new());
        match self {
            Pool::Build => &BUILD,
            Pool::Fill => &FILL,
            Pool::Refresh => &REFRESH,
        }
    }

    /// Runs `job` when a place in the pool is free.
    fn submit(self, job: PoolJob) {
        let mut state = self.state().lock();
        if state.running >= self.cap() {
            state.queue.push_back(job);
            return;
        }
        state.running += 1;
        drop(state);
        self.spawn(job);
    }

    /// A thread that runs `job`, then whatever queued behind it, until the
    /// queue is empty.
    fn spawn(self, job: PoolJob) {
        let spawned = std::thread::Builder::new()
            .name(format!("aft-data-{self:?}").to_lowercase())
            .spawn(move || {
                let mut next = Some(job);
                while let Some(job) = next.take() {
                    job();
                    if matches!(self, Pool::Build) {
                        // A build frees most of what it allocated when it
                        // ends. Hand those pages back now, as dead code does
                        // after a run, rather than holding them until the
                        // daemon's periodic relief comes round.
                        #[cfg(any(target_os = "linux", target_os = "macos"))]
                        let _ = crate::memory::relieve_allocator_pressure();
                    }
                    let mut state = self.state().lock();
                    next = state.queue.pop_front();
                    if next.is_none() {
                        state.running -= 1;
                    }
                }
            });
        if let Err(error) = spawned {
            self.state().lock().running -= 1;
            crate::slog_warn!("data layer could not start a {self:?} worker: {error}");
        }
    }
}

type PoolJob = Box<dyn FnOnce() + Send + 'static>;

struct PoolState {
    running: usize,
    queue: std::collections::VecDeque<PoolJob>,
}

impl PoolState {
    const fn new() -> Self {
        Self {
            running: 0,
            queue: std::collections::VecDeque::new(),
        }
    }
}

/// Where a checkout's background work runs. Under the daemon the root's
/// executor admits it first, so a root that is unbound starts none, and the
/// work then runs in a [`Pool`], never on an executor worker. A build, a fill
/// round or a refresh can take seconds to minutes; run as executor work it
/// held the root's lanes and the executor's heavy-build places for that long,
/// and every tool call on the root, and every other root's build, waited
/// behind it. The pool job carries the root's cancellation, so a root that
/// unbinds still stops it at its next checkpoint. A standalone AFT has no
/// executor and serves one root that is always bound, so there the work goes
/// straight to its pool.
struct Runner {
    root: PathBuf,
    executor: parking_lot::Mutex<Option<RootMaintenanceExecutor>>,
}

impl Runner {
    /// Runs `work` in `pool`. Work the executor refuses (the root is
    /// unbound, or the executor shut down) is dropped unrun, so anything it
    /// captured to mark itself running is released with it. Never falls back
    /// to running unadmitted under the daemon: that would put a retired root
    /// to work.
    fn run(&self, request_id: &'static str, pool: Pool, work: impl FnOnce() + Send + 'static) {
        let executor = self.executor.lock().clone();
        let Some(executor) = executor else {
            pool.submit(Box::new(work));
            return;
        };
        let cancellation = JobCancellation::new().with_root(&self.root);
        // The admission job only hands the work to its pool, so it holds the
        // root's maintenance lane for no longer than that.
        executor.submit_maintenance(
            request_id,
            Lane::MaintenanceCommit,
            cancellation,
            Box::new(move |_| {
                // The executor gave the admitted job the root's lifecycle; a
                // fresh token with the same root and lifecycle lets the pool
                // job see the root unbind at its checkpoints.
                let token = crate::executor::current_job_cancellation().map(|token| token.fresh());
                pool.submit(Box::new(move || {
                    let _cancel = token.map(crate::executor::install_job_cancellation);
                    work();
                }));
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
    /// A fold of the checkout's segments is queued or running.
    folding: Arc<AtomicBool>,
    /// A call-graph refresh is queued or running.
    refreshing: Arc<AtomicBool>,
    /// Timer requests survive a refresh already running when they arrive.
    publish_requested: parking_lot::Mutex<Publish>,
    settle: Arc<QuietTimer>,
    many_since: parking_lot::Mutex<Option<Instant>>,
    #[cfg(test)]
    publication_delay: parking_lot::Mutex<Duration>,
    #[cfg(test)]
    publications_started: std::sync::atomic::AtomicUsize,
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

#[cfg(test)]
fn build_holds_for_test() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static HOLDS: OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    HOLDS.get_or_init(Default::default)
}

/// Holds every build of the checkout at `root` before it starts, until the
/// returned guard drops, so a test can look at a checkout that is building.
#[cfg(test)]
pub(crate) fn hold_builds_for_test(root: &Path) -> BuildHold {
    build_holds_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf());
    BuildHold(root.to_path_buf())
}

/// See [`hold_builds_for_test`].
#[cfg(test)]
pub(crate) struct BuildHold(PathBuf);

#[cfg(test)]
impl Drop for BuildHold {
    fn drop(&mut self) {
        build_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
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
        self.runner.run(BUILD_REQUEST, Pool::Build, move || {
            #[cfg(test)]
            while build_holds_for_test()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(work.checkout.root())
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let result = work.checkout.build();
            drop(flag);
            work.checkout.notify_readiness_change();
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
                        "data layer built {}: {} files, {} parsed, {} bytes; shared with other \
                         checkouts: {} text units, {} records, {} fragments",
                        work.checkout.root().display(),
                        report.files,
                        report.parsed_files,
                        report.segment_bytes,
                        report.shared.text_units,
                        report.shared.records,
                        report.shared.fragments
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
        let unpublished = publish != Publish::WhenMany && self.checkout.changed_paths() > 0;
        if !(pending(&self.checkout) || unpublished) {
            return;
        }
        {
            let mut requested = self.publish_requested.lock();
            *requested = (*requested).max(publish);
        }
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = ClearOnDrop(Arc::clone(&self.refreshing));
        let work = Arc::clone(self);
        // A refresh changes only the checkout, behind its own lock, so it
        // runs beside the root's reads and edits, one at a time per root
        // (the `refreshing` flag), in the refresh pool.
        self.runner.run(REFRESH_REQUEST, Pool::Refresh, move || {
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
                    Ok(_) => {
                        let publish = std::mem::replace(
                            &mut *work.publish_requested.lock(),
                            Publish::WhenMany,
                        );
                        if pending(&work.checkout) {
                            let mut requested = work.publish_requested.lock();
                            *requested = (*requested).max(publish);
                            continue;
                        }
                        work.maybe_publish(publish);
                    }
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
                if !(pending(&work.checkout) || *work.publish_requested.lock() != Publish::WhenMany)
                    || work.refreshing.swap(true, Ordering::AcqRel)
                {
                    return;
                }
                flag = Some(ClearOnDrop(Arc::clone(&work.refreshing)));
            }
        });
    }

    /// Publishes the changes a refresh resolved, when `publish` asks for it,
    /// and folds the checkout's segments once publications of changes have
    /// piled up.
    fn maybe_publish(self: &Arc<Self>, publish: Publish) {
        let changed = self.checkout.changed_paths();
        let refreshed = self.checkout.refreshed_files();
        let many = changed >= PUBLISH_AT || refreshed >= PUBLISH_AT_REFRESHED;
        let capped = {
            let mut since = self.many_since.lock();
            if many {
                since.get_or_insert_with(Instant::now).elapsed() >= PUBLISH_MAX_WAIT
                    || refreshed >= PUBLISH_MAX_REFRESHED
            } else {
                *since = None;
                false
            }
        };
        if changed == 0 || (publish != Publish::Always && !many) {
            return;
        }
        if publish != Publish::Always
            && !capped
            && (publish == Publish::WhenMany || !self.settle.due())
        {
            self.settle.arm();
            return;
        }
        // A change can arrive after the worker's final refresh, including
        // while a timer request is being handled. Never start with it pending.
        if pending(&self.checkout) {
            let mut requested = self.publish_requested.lock();
            *requested = (*requested).max(publish);
            return;
        }
        #[cfg(test)]
        {
            self.publications_started.fetch_add(1, Ordering::SeqCst);
            let delay = *self.publication_delay.lock();
            std::thread::sleep(delay);
        }
        match self.checkout.publish_changes() {
            Ok(Some(report)) => {
                *self.many_since.lock() = None;
                crate::slog_info!(
                    "data layer published changes for {}: {} paths, {} fragments, {} text units, {} segments ({} read in all, {} retired), {:.0} ms",
                    self.checkout.root().display(),
                    report.paths,
                    report.fragments,
                    report.text_units,
                    report.segments,
                    report.read_segments,
                    report.retired_segments,
                    report.ms
                );
                self.after_publish(&report);
            }
            Ok(None) | Err(DataError::Cancelled) => {}
            Err(error) => crate::slog_warn!(
                "data layer could not publish changes for {}: {error}",
                self.checkout.root().display()
            ),
        }
    }

    /// Folds the checkout's segments when a publication of its changes left
    /// it reading from too many, and otherwise looks for another checkout
    /// of the family that is due a fold and has not published to get one
    /// (see [`Checkout::fold_other`]): at most one fold per publication.
    fn after_publish(self: &Arc<Self>, report: &DeltaReport) {
        if report.wants_fold() {
            self.start_fold();
        } else {
            self.start_fold_other();
        }
    }

    /// Folds, on its behalf, the other checkout of the family most due a
    /// fold, unless a fold of this root is queued or running already. It
    /// runs in the build pool and takes the store's writer as a fold does.
    fn start_fold_other(self: &Arc<Self>) {
        if self.folding.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = ClearOnDrop(Arc::clone(&self.folding));
        let work = Arc::clone(self);
        self.runner.run(FOLD_REQUEST, Pool::Build, move || {
            if work.building.load(Ordering::Acquire) {
                return;
            }
            let result = work.checkout.fold_other();
            drop(flag);
            match result {
                Ok(report) => match (&report.fold, &report.skipped) {
                    (Some(fold), _) => crate::slog_info!(
                        "data layer folded {} on behalf of {}: examined {} checkouts in {:.0} ms; it read {} segments only it reads, {} in all; {} segments into one of {} bytes ({} text units, {} records, {} fragments; shared with other checkouts: {} text units, {} records, {} fragments; {} retired), {:.0} ms",
                        report.root.as_deref().unwrap_or(std::path::Path::new("?")).display(),
                        work.checkout.root().display(),
                        report.examined,
                        report.examine_ms,
                        report.own_segments,
                        report.read_segments,
                        fold.read_before,
                        fold.segment_bytes,
                        fold.text_units,
                        fold.records,
                        fold.fragments,
                        fold.shared.text_units,
                        fold.shared.records,
                        fold.shared.fragments,
                        fold.retired_segments,
                        report.ms
                    ),
                    (None, Some(why)) => crate::slog_info!(
                        "data layer cannot fold {} on behalf of {} ({why}); examined {} checkouts in {:.0} ms",
                        report.root.as_deref().unwrap_or(std::path::Path::new("?")).display(),
                        work.checkout.root().display(),
                        report.examined,
                        report.examine_ms
                    ),
                    (None, None) => crate::slog_debug!(
                        "data layer: no other checkout of {} is due a fold; examined {} checkouts in {:.0} ms",
                        work.checkout.root().display(),
                        report.examined,
                        report.examine_ms
                    ),
                },
                Err(DataError::Cancelled) => {}
                Err(error) => crate::slog_warn!(
                    "data layer could not fold another checkout on behalf of {}: {error}",
                    work.checkout.root().display()
                ),
            }
        });
    }

    /// Folds the segments the checkout reads into one, unless a fold is
    /// queued or running already. It runs in the build pool and takes the
    /// store's writer as a build does, but reads no file and parses
    /// nothing (see [`Checkout::fold`]). When the store cannot give what
    /// it needs, the checkout is built again whole instead.
    fn start_fold(self: &Arc<Self>) {
        if self.folding.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = ClearOnDrop(Arc::clone(&self.folding));
        let work = Arc::clone(self);
        self.runner.run(FOLD_REQUEST, Pool::Build, move || {
            if work.building.load(Ordering::Acquire) {
                // A build queued or running writes everything again anyway.
                return;
            }
            let result = work.checkout.fold();
            drop(flag);
            match result {
                Ok(Folded::Done(report)) => {
                    crate::slog_info!(
                        "data layer folded {}: {} segments into one of {} bytes ({} text units, {} records, {} fragments; shared with other checkouts: {} text units, {} records, {} fragments; {} retired), {:.0} ms",
                        work.checkout.root().display(),
                        report.read_before,
                        report.segment_bytes,
                        report.text_units,
                        report.records,
                        report.fragments,
                        report.shared.text_units,
                        report.shared.records,
                        report.shared.fragments,
                        report.retired_segments,
                        report.ms
                    );
                    work.notify_status_change();
                    // Changes recorded meanwhile are resolved again over the
                    // new publication.
                    work.start_refresh(Publish::WhenMany);
                }
                Ok(Folded::Superseded) => {
                    work.notify_status_change();
                    work.start_refresh(Publish::WhenMany);
                }
                Ok(Folded::NeedsBuild(why)) => {
                    crate::slog_info!(
                        "data layer cannot fold the segments of {} ({why}); building it again whole",
                        work.checkout.root().display()
                    );
                    work.start_build();
                }
                Err(DataError::Cancelled) => {}
                // The next publication that leaves too many segments tries
                // again.
                Err(error) => crate::slog_warn!(
                    "data layer could not fold the segments of {}: {error}",
                    work.checkout.root().display()
                ),
            }
        });
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
            || self.folding.load(Ordering::Acquire)
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
        self.runner.run(RESCAN_REQUEST, Pool::Refresh, move || {
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
            work.settle.note_change();
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
        if !self.meaning.enabled() || self.meaning.reads_only() {
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
        self.runner.run(FILL_REQUEST, Pool::Fill, move || {
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
                    let totals = work.meaning.totals();
                    crate::slog_info!(
                        "data layer meaning fill finished for {}: {} files, {} chunks taken from the old meaning index, {} chunks embedded",
                        work.checkout.root().display(),
                        totals.files,
                        totals.chunks_imported,
                        totals.chunks_embedded
                    );
                }
                Round::Idle => {}
                Round::Failed(failure) => work.retry_fill_later(failure.message()),
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
        self.checkout.notify_readiness_change();
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
/// How old a checkout's memory figure may be before the census walks it
/// again (see [`DataCheckout::memory_figure`]).
const MEMORY_FIGURE_MAX_AGE: Duration = Duration::from_secs(30);

pub(crate) struct DataCheckout {
    work: Arc<Work>,
    quiet: Arc<QuietTimer>,
    /// Set once a bind has used the scan that opening the checkout made;
    /// every later bind compares the files again (see [`root_bound`]).
    bound_once: AtomicBool,
    /// Whether the last [`Self::take_todos_readable`] found the checkout's
    /// TODO markers readable.
    todos_readable: AtomicBool,
    /// The last memory figure the census walked for this checkout, and when
    /// (see [`Self::memory_figure`]).
    memory_figure: std::sync::Mutex<Option<(Instant, u64, super::surfaces::DataLayerMemory)>>,
    /// Kept so the edit-intent listener lives as long as the checkout.
    _listener: Arc<dyn WriteIntentListener>,
}

impl Drop for DataCheckout {
    // An evicted checkout must not leave its quiet-timer thread asleep for
    // up to `QUIET`: its refresh would find nothing to refresh anyway.
    fn drop(&mut self) {
        self.quiet.stop();
        self.work.settle.stop();
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
        Self::open_with_periods(
            root,
            storage,
            executor,
            planes,
            quiet_period(),
            settle_period(),
        )
    }

    fn open_with_periods(
        root: &Path,
        storage: &Path,
        executor: Option<RootMaintenanceExecutor>,
        planes: Planes,
        quiet_period: Duration,
        settle_period: Duration,
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
        let work = Arc::new_cyclic(|weak: &Weak<Work>| Work {
            runner: Runner {
                root: checkout.root().to_path_buf(),
                executor: parking_lot::Mutex::new(executor),
            },
            checkout,
            building: Arc::new(AtomicBool::new(false)),
            folding: Arc::new(AtomicBool::new(false)),
            refreshing: Arc::new(AtomicBool::new(false)),
            publish_requested: parking_lot::Mutex::new(Publish::WhenMany),
            settle: Arc::new(QuietTimer::new(settle_period, {
                let work = weak.clone();
                move || {
                    if let Some(work) = work.upgrade() {
                        work.start_refresh(Publish::Settled);
                    }
                }
            })),
            many_since: parking_lot::Mutex::new(None),
            #[cfg(test)]
            publication_delay: parking_lot::Mutex::new(Duration::ZERO),
            #[cfg(test)]
            publications_started: std::sync::atomic::AtomicUsize::new(0),
            rescanning: Arc::new(AtomicBool::new(false)),
            retry: std::sync::Mutex::default(),
            status_listener: std::sync::Mutex::new(None),
            meaning: Meaning::new(storage),
            filling: Arc::new(AtomicBool::new(false)),
            fill_retry: std::sync::Mutex::default(),
        });
        let quiet = {
            let work = Arc::downgrade(&work);
            let timer = QuietTimer::new(quiet_period, move || {
                if let Some(work) = work.upgrade() {
                    work.start_refresh(Publish::Always);
                    // Edited files wait for vectors of their new content.
                    work.start_fill();
                }
            });
            #[cfg(feature = "test-timing-hooks")]
            let timer = timer.with_test_gate();
            Arc::new(timer)
        };
        let listener: Arc<dyn WriteIntentListener> = Arc::new(EditListener {
            checkout: Arc::downgrade(&work.checkout),
            root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            quiet: Arc::clone(&quiet),
            settle: Arc::clone(&work.settle),
        });
        crate::write_intent::register_listener(&listener);
        // A bind may have recorded what changed while nothing watched.
        work.catch_up(Publish::WhenMany);
        let checkout = Arc::new(Self {
            work,
            quiet,
            bound_once: AtomicBool::new(false),
            todos_readable: AtomicBool::new(false),
            memory_figure: std::sync::Mutex::new(None),
            _listener: listener,
        });
        super::bound_checkouts::note_bound(&checkout);
        Ok(checkout)
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

    /// Only a queued or running first text build warrants waiting. An old
    /// publication, a failure, or a checkout with no build scheduled does not.
    pub(crate) fn first_text_building(&self) -> bool {
        self.work.building.load(Ordering::Acquire)
            && self.work.checkout.planes().text
            && self.work.checkout.view().is_none()
            && self.work.checkout.last_failure().is_none()
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
        self.work.settle.note_change();
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
    #[cfg(feature = "test-timing-hooks")]
    test_gate: bool,
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
            #[cfg(feature = "test-timing-hooks")]
            test_gate: false,
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

    #[cfg(feature = "test-timing-hooks")]
    fn with_test_gate(mut self) -> Self {
        self.test_gate = true;
        self
    }

    /// Hold only the semantic quiet timer, not publication's settle timer.
    /// Changes continue moving its deadline while the integration test writes
    /// and removes a burst; the deadline is checked again after release.
    #[cfg(feature = "test-timing-hooks")]
    fn hold_for_test(&self) -> Option<PathBuf> {
        if !self.test_gate {
            return None;
        }
        let gate = PathBuf::from(std::env::var_os("AFT_TEST_DATA_QUIET_GATE")?);
        if !gate.exists() {
            return None;
        }
        std::fs::write(gate.with_extension("waiting"), b"waiting").expect("signal quiet gate");
        while gate.exists() && !gate.with_extension("release").exists() {
            if self.sleep(Duration::from_millis(10)) {
                return None;
            }
        }
        Some(gate)
    }

    /// Notes a recorded change, and starts the timer thread unless one is
    /// waiting already.
    fn changed(self: &Arc<Self>) {
        self.note_change();
        self.arm();
    }

    /// Move an existing deadline without arming an idle timer. Publications
    /// only need a settle timer once enough unpublished work has accumulated.
    fn note_change(&self) {
        self.last_change_ms.store(self.now_ms(), Ordering::Release);
    }

    fn due(&self) -> bool {
        self.now_ms()
            .saturating_sub(self.last_change_ms.load(Ordering::Acquire))
            >= u64::try_from(self.quiet.as_millis()).unwrap_or(u64::MAX)
    }

    /// Wait from the last recorded change, not from the end of its refresh.
    fn arm(self: &Arc<Self>) {
        if self.sleep(Duration::ZERO) || self.armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let timer = Arc::clone(self);
        let quiet_ms = u64::try_from(self.quiet.as_millis()).unwrap_or(u64::MAX);
        let spawned = std::thread::Builder::new()
            .name("aft-data-quiet".to_string())
            .spawn(move || {
                #[cfg(feature = "test-timing-hooks")]
                let gate = timer.hold_for_test();
                if timer.sleep(Duration::ZERO) {
                    return;
                }
                loop {
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
                    #[cfg(feature = "test-timing-hooks")]
                    if let Some(gate) = &gate {
                        std::fs::write(gate.with_extension("settled"), b"settled")
                            .expect("signal quiet fire");
                    }
                    break;
                }
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
    use super::bound_checkouts::{note_root, RootKind};
    if let Some(root) = ctx.canonical_cache_root_opt() {
        // A folder of repositories is checked first: configure switches its
        // own indexes off, which alone would withhold it.
        let kind = if crate::parent_search::session_for(ctx).is_some() {
            RootKind::Repositories
        } else if ctx.data_layer_withheld().is_some() {
            RootKind::Withheld
        } else {
            RootKind::Own
        };
        note_root(&root, kind);
    }
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
    settle: Arc<QuietTimer>,
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
        self.settle.note_change();
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

/// Whether `storage` already holds the family store (the data layer's store
/// shared by every checkout of one repository) of the repository at `root`:
/// binding a checkout of a repository with none would make one. `false` when
/// Git could not tell the repository's identity.
pub(crate) fn has_family_store(root: &Path, storage: &Path) -> bool {
    family_of(root).is_ok_and(|family| {
        storage
            .join("data")
            .join(family.digest().to_string())
            .is_dir()
    })
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

    /// Whether the checkout's TODO markers became readable since the last
    /// call: its call graph is published now and no build or rescan is
    /// running, where at the last call that was not so. The status listener
    /// asks this on every status change, so a TODO count that came back
    /// pending while the store built is taken again once the store has
    /// published, and only once per such change.
    pub(crate) fn take_todos_readable(&self) -> bool {
        let readable = !self.work.checkout.status().building
            && self
                .view()
                .is_some_and(|view| view.status().callgraph_published && !view.rescanning());
        let was_readable = self.todos_readable.swap(readable, Ordering::AcqRel);
        readable && !was_readable
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
    /// none has learned the model yet.
    pub(crate) fn meaning(
        &self,
        config: impl FnOnce() -> Option<crate::config::SemanticBackendConfig>,
    ) -> MeaningState {
        self.work.meaning.configure_once(config);
        self.work.meaning.refresh_read_only();
        let state = self.meaning_status();
        let wanted = match &state {
            MeaningState::Off | MeaningState::Unpublished => false,
            MeaningState::Starting | MeaningState::Blocked(_) => true,
            MeaningState::Serving { snapshot, .. } => super::meaning::waiting(snapshot.coverage()),
        };
        if wanted {
            self.work.start_fill();
        }
        state
    }

    /// How meaning search stands for the current view, for status reads:
    /// starts nothing.
    pub(crate) fn meaning_status(&self) -> MeaningState {
        self.work.meaning.state(self.view().as_ref())
    }

    /// Makes this checkout's meaning search read only the vectors its
    /// repository's store already holds for `config`'s model (identified by
    /// `fingerprint`), and never fill: for a child repository of a parent
    /// folder's session. See [`super::meaning::Meaning::read_only`].
    ///
    /// # Errors
    ///
    /// When the plane cannot be made.
    pub(crate) fn read_meaning_only(
        &self,
        config: &crate::config::SemanticBackendConfig,
        fingerprint: &crate::semantic_index::SemanticIndexFingerprint,
    ) -> Result<(), String> {
        self.work
            .meaning
            .read_only(&self.work.checkout, config, fingerprint)
    }

    /// Whether a fill round is queued or running.
    pub(crate) fn filling(&self) -> bool {
        self.work.filling.load(Ordering::Acquire)
    }

    /// Whether meaning search has finished a fill under its current
    /// configuration, so pending files are refreshes, not a first build.
    pub(crate) fn meaning_filled(&self) -> bool {
        self.work.meaning.filled()
    }

    /// What the last fill round reported, if a fill ran.
    pub(crate) fn meaning_progress(&self) -> Option<super::vectors::FillProgress> {
        self.work.meaning.progress()
    }

    /// When meaning search's fill under way started and when a fill last
    /// stored what it filled.
    pub(crate) fn meaning_fill_times(&self) -> super::meaning::FillTimes {
        self.work.meaning.times()
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
    /// This checkout's memory as the census reports it, walked again only
    /// when the checkout changed what it holds since the last walk, or the
    /// last figure is older than [`MEMORY_FIGURE_MAX_AGE`]. The health
    /// rollup asks for every bound root every few seconds, and the walk
    /// visits each decoded parse and resolution an edited checkout's engine
    /// holds: walked on every ask, it kept a core busy.
    pub(crate) fn memory_figure(
        &self,
        walk: impl FnOnce() -> super::surfaces::DataLayerMemory,
    ) -> super::surfaces::DataLayerMemory {
        // Another thread walking it now holds the slot: report it busy
        // rather than wait.
        let Ok(mut slot) = self.memory_figure.try_lock() else {
            return super::surfaces::DataLayerMemory::busy_checkout();
        };
        // Read before the walk: a change during it moves the epoch, so the
        // next ask walks again.
        let epoch = self.checkout().memory_epoch();
        if let Some((at, walked_at, figure)) = slot.as_ref() {
            if *walked_at == epoch && at.elapsed() < MEMORY_FIGURE_MAX_AGE {
                return figure.clone();
            }
        }
        let figure = walk();
        *slot = Some((Instant::now(), epoch, figure.clone()));
        figure
    }

    pub(crate) fn meaning_memory(&self, seen: &mut aft_store::heap::Seen) -> Option<usize> {
        self.work.meaning.memory(seen)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    struct PublicationFixture {
        handle: Arc<DataCheckout>,
        _root: tempfile::TempDir,
        _storage: tempfile::TempDir,
    }

    impl PublicationFixture {
        fn new(settle: Duration, quiet: Duration) -> Self {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let path = std::fs::canonicalize(root.path()).unwrap();
            std::fs::create_dir_all(path.join("src")).unwrap();
            std::fs::write(path.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
            std::fs::write(path.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
            let handle = DataCheckout::open_with_periods(
                &path,
                storage.path(),
                None,
                Planes::ALL,
                quiet,
                settle,
            )
            .unwrap();
            assert!(matches!(
                handle.fresh_view(Duration::from_secs(60)),
                Fresh::Ready(_)
            ));
            wait_until(|| !handle.work.running());
            Self {
                handle,
                _root: root,
                _storage: storage,
            }
        }

        fn add(&self, stem: &str, count: usize) {
            let paths = (0..count)
                .map(|i| {
                    let path = self.handle.root().join(format!("src/{stem}{i}.rs"));
                    std::fs::write(&path, format!("pub fn {stem}{i}() {{}}\n")).unwrap();
                    path
                })
                .collect::<Vec<_>>();
            self.handle.note_paths(&paths);
        }

        fn refresh(&self) {
            assert!(matches!(
                self.handle.fresh_view(Duration::from_secs(60)),
                Fresh::Ready(_)
            ));
            wait_until(|| !self.handle.work.refreshing.load(Ordering::Acquire));
        }

        fn publications(&self) -> usize {
            self.handle.work.publications_started.load(Ordering::SeqCst)
        }
    }

    fn wait_until(ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready() {
            assert!(Instant::now() < deadline, "background work did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_next_edit_gets_a_fresh_view_before_a_slow_publication() {
        let fixture = PublicationFixture::new(Duration::from_secs(60), QUIET);
        *fixture.handle.work.publication_delay.lock() = Duration::from_secs(2);
        fixture.add("threshold", PUBLISH_AT);
        fixture.handle.request_refresh();
        // On the old inline path the publication starts and keeps the flag;
        // the deferred path lets go of it without starting a publication.
        wait_until(|| {
            fixture.publications() > 0 || !fixture.handle.work.refreshing.load(Ordering::Acquire)
        });
        fixture.add("next_edit", 1);
        assert!(
            matches!(
                fixture.handle.fresh_view(Duration::from_secs(1)),
                Fresh::Ready(_)
            ),
            "the next edit waited behind the previous edit's publication"
        );
        fixture.refresh();
        assert_eq!(fixture.publications(), 0);
        assert_eq!(fixture.handle.checkout().changed_paths(), PUBLISH_AT + 1);
    }

    #[test]
    fn a_threshold_publication_waits_for_the_last_change_to_settle() {
        let settle = Duration::from_millis(400);
        let fixture = PublicationFixture::new(settle, QUIET);
        fixture.add("threshold", PUBLISH_AT);
        fixture.refresh();
        assert_eq!(fixture.publications(), 0);
        // Keep moving the deadline past its original due time, without
        // asking for another refresh. The timer must refresh this edit too.
        for i in 0..3 {
            std::thread::sleep(Duration::from_millis(150));
            fixture.add(&format!("later{i}"), 1);
            assert_eq!(fixture.publications(), 0);
        }
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            fixture.publications(),
            0,
            "published before the last change settled"
        );
        wait_until(|| fixture.handle.checkout().changed_paths() == 0);
        assert_eq!(fixture.publications(), 1);
        assert!(!pending(fixture.handle.checkout()));
        let view = fixture.handle.view().unwrap();
        assert_eq!(
            view.probe_query("callers", "src/later20.rs", "later20"),
            Ok(0)
        );
    }

    #[test]
    fn continuous_edits_publish_at_the_time_cap() {
        let fixture = PublicationFixture::new(Duration::from_secs(60), QUIET);
        fixture.add("threshold", PUBLISH_AT);
        fixture.refresh();
        let since = fixture.handle.work.many_since.lock().unwrap();
        for i in 0..3 {
            fixture.add(&format!("continued{i}"), 1);
            fixture.refresh();
            assert_eq!(*fixture.handle.work.many_since.lock(), Some(since));
            assert_eq!(fixture.publications(), 0);
        }
        // Advance only the accumulated-memory age, not the settle deadline.
        *fixture.handle.work.many_since.lock() = Some(Instant::now() - PUBLISH_MAX_WAIT);
        fixture.add("after_cap", 1);
        fixture.refresh();
        assert_eq!(fixture.publications(), 1);
        assert_eq!(fixture.handle.checkout().changed_paths(), 0);
        assert!(fixture.handle.work.many_since.lock().is_none());
    }

    #[test]
    fn resolution_memory_publishes_at_the_file_cap() {
        let fixture = PublicationFixture::new(Duration::from_secs(60), QUIET);
        fixture.add("resolved", PUBLISH_MAX_REFRESHED);
        fixture.refresh();
        assert_eq!(fixture.publications(), 1);
        assert_eq!(fixture.handle.checkout().changed_paths(), 0);
        assert_eq!(fixture.handle.checkout().refreshed_files(), 0);
    }

    #[test]
    fn quiet_changes_still_refresh_and_publish_below_the_threshold() {
        let fixture = PublicationFixture::new(Duration::from_secs(60), Duration::from_millis(200));
        fixture.add("quiet_edit", 1);
        assert_eq!(fixture.publications(), 0);
        wait_until(|| fixture.handle.checkout().changed_paths() == 0);
        assert_eq!(fixture.publications(), 1);
        assert!(matches!(
            fixture.handle.fresh_view(Duration::ZERO),
            Fresh::Ready(_)
        ));
    }

    #[test]
    fn a_settled_request_during_a_refresh_is_not_lost() {
        let fixture = PublicationFixture::new(Duration::from_millis(100), QUIET);
        fixture.add("threshold", PUBLISH_AT);
        // Own the refresh flag as a worker would while resolving changes.
        fixture
            .handle
            .work
            .refreshing
            .store(true, Ordering::Release);
        fixture.handle.work.settle.arm();
        wait_until(|| *fixture.handle.work.publish_requested.lock() == Publish::Settled);
        fixture
            .handle
            .work
            .refreshing
            .store(false, Ordering::Release);
        fixture.refresh();
        assert_eq!(fixture.publications(), 1);
        assert_eq!(fixture.handle.checkout().changed_paths(), 0);
    }

    #[test]
    fn a_new_edit_postpones_a_settled_request_already_waiting_for_refresh() {
        let fixture = PublicationFixture::new(Duration::from_millis(400), QUIET);
        fixture.add("threshold", PUBLISH_AT);
        fixture
            .handle
            .work
            .refreshing
            .store(true, Ordering::Release);
        fixture.handle.work.settle.arm();
        wait_until(|| *fixture.handle.work.publish_requested.lock() == Publish::Settled);
        fixture.add("after_timer", 1);
        fixture
            .handle
            .work
            .refreshing
            .store(false, Ordering::Release);
        fixture.refresh();
        assert_eq!(
            fixture.publications(),
            0,
            "a stale settle request published a new edit"
        );
        wait_until(|| fixture.handle.checkout().changed_paths() == 0);
        assert_eq!(fixture.publications(), 1);
    }

    #[test]
    fn publication_refreshes_pending_changes_before_it_starts() {
        let fixture = PublicationFixture::new(Duration::from_secs(60), QUIET);
        fixture.add("threshold", PUBLISH_AT);
        fixture.refresh();
        fixture.add("pending_edit", 1);
        fixture.handle.work.maybe_publish(Publish::Always);
        assert_eq!(fixture.publications(), 0);
        assert!(pending(fixture.handle.checkout()));
        fixture.refresh();
        assert_eq!(fixture.publications(), 1);
        assert_eq!(fixture.handle.checkout().changed_paths(), 0);
    }

    /// A process published a checkout's text index and stopped before its
    /// call graph. The next process to bind it builds the call graph, and
    /// its refresh thread does not spin while it waits.
    #[test]
    fn the_memory_figure_is_walked_again_only_after_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn caller() {}\n").unwrap();
        let handle = DataCheckout::open(&root, storage.path(), None).unwrap();
        let walks = std::cell::Cell::new(0);
        let walk = || {
            walks.set(walks.get() + 1);
            super::super::surfaces::DataLayerMemory::busy_checkout()
        };
        handle.memory_figure(walk);
        handle.memory_figure(walk);
        assert_eq!(walks.get(), 1, "an unchanged checkout is not walked again");
        std::fs::write(root.join("src/extra.rs"), "pub fn other() {}\n").unwrap();
        handle
            .checkout()
            .record_paths(&[b"src/extra.rs".to_vec()])
            .unwrap();
        handle.memory_figure(walk);
        assert_eq!(walks.get(), 2, "a recorded change is measured at once");
    }

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

    /// Background work the executor admits runs in its pool, not on an
    /// executor worker, so it holds neither the root's lanes nor another
    /// pool's places. Two fill rounds stuck on an embedding backend used to
    /// hold both heavy-build places and the root's maintenance for minutes:
    /// builds of other roots waited behind them, and the root's tool calls
    /// queued for up to a minute.
    #[test]
    fn admitted_work_runs_off_the_executor_and_blocks_no_other_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = Arc::new(crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ));
        ctx.mark_subc_bound();
        let executor = crate::executor::Executor::new();
        let root_id = crate::path_identity::ProjectRootId::from_path(&root).unwrap();
        assert!(executor.register_actor(root_id.clone(), Arc::clone(&ctx)));
        let maintenance = ctx
            .callgraph_executor_for_test()
            .expect("registering the actor installs its executor");
        let runner = Runner {
            root: root.clone(),
            executor: parking_lot::Mutex::new(Some(maintenance)),
        };

        // Fill every place in the fill pool with rounds that wait until the
        // test lets them go.
        let release = Arc::new(std::sync::Barrier::new(Pool::Fill.cap() + 1));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        for _ in 0..Pool::Fill.cap() {
            let release = Arc::clone(&release);
            let started = started_tx.clone();
            runner.run(FILL_REQUEST, Pool::Fill, move || {
                started
                    .send(crate::executor::current_thread_is_executor_worker())
                    .unwrap();
                release.wait();
            });
        }
        for _ in 0..Pool::Fill.cap() {
            let on_worker = started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(!on_worker, "a fill round ran on an executor worker");
        }

        // A build still starts while every fill is stuck.
        let (built_tx, built_rx) = std::sync::mpsc::channel();
        runner.run(BUILD_REQUEST, Pool::Build, move || {
            built_tx
                .send(crate::executor::current_thread_is_executor_worker())
                .unwrap();
        });
        let on_worker = built_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a build waited behind stuck fill rounds");
        assert!(!on_worker, "a build ran on an executor worker");

        // And a tool call on the same root runs at once.
        let started = Instant::now();
        let response = executor
            .submit(
                root_id,
                Lane::PureRead,
                "read".to_string(),
                Box::new(|_| crate::protocol::Response::success("read", serde_json::Value::Null)),
            )
            .recv_timeout(Duration::from_secs(10))
            .expect("a read on the root never ran");
        assert!(response.success);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a read on the root waited {:?} behind its background work",
            started.elapsed()
        );
        release.wait();
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

    /// Once publications of a checkout's changes leave it reading from too
    /// many segments, the runtime folds them into one from what the store
    /// holds. It used to build the checkout again whole, which read every
    /// file and resolved the whole call graph again to write units the
    /// store already held.
    #[test]
    fn too_many_segments_are_folded_without_reading_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn caller() {}\n").unwrap();
        let handle = DataCheckout::open(&root, storage.path(), None).unwrap();
        if let Fresh::Building | Fresh::Failed(_) = handle.fresh_view(Duration::from_secs(60)) {
            panic!("the checkout was never built");
        }
        let checkout = handle.checkout();
        let mut report = None;
        // Each round adds a file nothing changes again, so each round's
        // segment stays read.
        for round in 0..=super::super::checkout::COMPACT_AT {
            let path = format!("src/added{round}.rs");
            std::fs::write(root.join(&path), format!("pub fn added{round}() {{}}\n")).unwrap();
            checkout.record_paths(&[path.into_bytes()]).unwrap();
            checkout.refresh_callgraph().unwrap();
            report = checkout.publish_changes().unwrap();
        }
        let report = report.expect("published");
        assert!(report.wants_fold(), "{report:?}");
        let segments = checkout.store_segments().unwrap();
        let work = super::super::checkout::source_work_for_test(&root);

        handle.work.after_publish(&report);
        let deadline = Instant::now() + Duration::from_secs(60);
        while checkout.store_segments().unwrap() == segments && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        while handle.work.running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(checkout.store_segments().unwrap(), 1, "{segments} segments");
        assert_eq!(
            super::super::checkout::source_work_for_test(&root),
            work,
            "the segments were folded by reading or resolving files"
        );
        let view = checkout.view().unwrap();
        assert_eq!(view.probe_query("callers", "src/lib.rs", "caller"), Ok(0));
        let query = aft_store::text::plan_pattern("added3", false).unwrap();
        assert_eq!(
            view.text_candidates(&query, aft_store::text::Scope::All)
                .unwrap(),
            vec![b"src/added3.rs".to_vec()]
        );
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
