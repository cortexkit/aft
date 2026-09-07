mod single_flight;

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Sender};
use parking_lot::{Condvar, Mutex, RwLock};
use tokio::sync::oneshot;

use crate::{context::AppContext, path_identity::ProjectRootId, protocol::Response};

pub use single_flight::SingleFlight;

const JOB_COST: isize = 1;
/// Continuous wake producers must not keep the scheduler mutex across an
/// unbounded receiver drain; health probes and submitters get a lock turn
/// between batches.
const SCHEDULER_EVENT_BATCH_CAP: usize = 64;
pub(crate) const MAINTENANCE_QUEUE_CAP: usize = 512;

/// Scheduler lane for command-handler execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// Pure read-only work. Runs under the actor epoch read gate and is capped
    /// per actor.
    PureRead,
    /// LSP/status work. Serialized per actor by scheduler admission while still
    /// using the shared epoch read gate.
    SerialLspStatus,
    /// Heavy lazy initialization. The scheduler acquires a process-wide heavy
    /// permit before dispatch; cache and index builders reject stale generations
    /// and serialize publication internally, so no actor epoch guard wraps the
    /// command or the delivery of its response.
    HeavyInit,
    /// Mutating work. Becomes a writer barrier at the actor queue head, drains
    /// in-flight reads, and runs under the actor epoch write gate. Reserved for
    /// configure and user-initiated tool mutations: background maintenance must
    /// use `MaintenanceCommit` so it can never exclude interactive reads.
    Mutating,
    /// Maintenance work that mutates only subsystem state behind that
    /// subsystem's own lock (watcher/LSP drains, completed-build installs,
    /// callgraph store writes). Runs under the actor epoch READ gate and
    /// overlaps PureReads; serialized to one in-flight per actor so
    /// maintenance cannot self-stack.
    MaintenanceCommit,
}

/// Scheduler class used to keep deferrable maintenance from occupying the
/// workers reserved for interactive route binds, tool calls, and bash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobClass {
    Interactive,
    Maintenance,
}

/// Idempotent maintenance drains that can share one queued execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MaintenanceCoalesceKey {
    WatcherDrain,
    LspDrain,
    /// Standing maintenance is coalesced inside each actor queue. The executor
    /// already scopes queue state by root, so this key cannot coalesce work for
    /// separate standing roots.
    StandingPass,
}

pub type ExecutorJob = Box<dyn FnOnce(&AppContext) -> Response + Send + 'static>;

/// Age at which a queued interactive mutating job (route binds, edits) jumps
/// ahead of pure reads in interactive admission. Reads-first admission would
/// otherwise let a sustained read stream starve queued writers; this bounds
/// that wait. For binds it matches the half-deadline breadcrumb point: the
/// daemon rejects binds at 12s, so promotion at 6s leaves half the budget for
/// draining readers and running the configure itself.
const INTERACTIVE_WRITER_PROMOTION_AGE: Duration = Duration::from_secs(6);
/// Readers older than this still block queued writer jobs; report their job
/// metadata instead of only the generic `waiting_on_readers` diagnosis.
const READER_STUCK_CENSUS_AGE: Duration = Duration::from_secs(60);
/// Stack for executor worker threads: the standalone main thread's size, so a
/// request that fits there cannot overflow only under the daemon. Stack pages
/// are committed on touch, so idle workers cost nothing extra.
const EXECUTOR_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub pool_size: usize,
    pub read_cap: usize,
    pub actor_cap: usize,
    pub heavy_permits: usize,
    pub drr_quantum: isize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        let available = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);
        let pool_size = available.saturating_sub(1).clamp(2, 8);
        let actor_cap = pool_size.saturating_sub(1).clamp(1, 4);
        let read_cap = actor_cap.clamp(1, 4);
        let heavy_permits = pool_size.saturating_sub(1).clamp(2, 3);

        Self {
            pool_size,
            read_cap,
            actor_cap,
            heavy_permits,
            drr_quantum: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct EffectiveConfig {
    pool_size: usize,
    read_cap: usize,
    actor_cap: usize,
    heavy_permits: usize,
    drr_quantum: isize,
    deficit_cap: isize,
    interactive_reserve: usize,
    maintenance_cap: usize,
}

impl ExecutorConfig {
    fn effective(&self) -> EffectiveConfig {
        let pool_size = self.pool_size.clamp(2, 8);
        let max_actor_cap = pool_size.saturating_sub(1).max(1);
        let actor_cap = self.actor_cap.max(1).min(max_actor_cap);
        let read_cap = self.read_cap.max(1).min(actor_cap).min(4);
        // HeavyInit jobs share workers with RouteBind/configure. Keep one worker
        // available even in a two-worker pool so a heavy-init storm cannot hold
        // a fresh bind behind every executor worker.
        let heavy_permits = self
            .heavy_permits
            .max(1)
            .min(pool_size.saturating_sub(1).max(1))
            .min(3);
        let drr_quantum = self.drr_quantum.max(1);
        let deficit_cap = (actor_cap.max(1) as isize) * 4;
        let interactive_reserve = if pool_size >= 4 { 2 } else { 1 };
        let maintenance_cap = pool_size.saturating_sub(interactive_reserve).max(1);

        EffectiveConfig {
            pool_size,
            read_cap,
            actor_cap,
            heavy_permits,
            drr_quantum,
            deficit_cap,
            interactive_reserve,
            maintenance_cap,
        }
    }
}

/// Synchronous completion handle used by the executor tests and the
/// future standalone bridge.
pub struct CompletionHandle {
    rx: Receiver<Response>,
}

impl CompletionHandle {
    pub fn recv(self) -> Result<Response, RecvError> {
        self.rx.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Response, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    pub fn into_receiver(self) -> Receiver<Response> {
        self.rx
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchClassQueueSnapshot {
    pub queued: usize,
    pub oldest_age_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRunningSnapshot {
    pub interactive: usize,
    pub maintenance: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchLivenessSnapshot {
    pub interactive: DispatchClassQueueSnapshot,
    pub maintenance: DispatchClassQueueSnapshot,
    pub running: DispatchRunningSnapshot,
    pub interactive_reserve: usize,
    pub maintenance_cap: usize,
}

/// Scheduler-owned mirror read by health probes without taking the actor map
/// lock or scanning every root. Oldest queue timestamps use `0` for absent and
/// elapsed-milliseconds-plus-one for a present timestamp.
struct DispatchLivenessAtomics {
    origin: Instant,
    interactive_queued: AtomicUsize,
    interactive_oldest_enqueued_ms_plus_one: AtomicU64,
    maintenance_queued: AtomicUsize,
    maintenance_oldest_enqueued_ms_plus_one: AtomicU64,
    interactive_running: AtomicUsize,
    maintenance_running: AtomicUsize,
}

impl DispatchLivenessAtomics {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            interactive_queued: AtomicUsize::new(0),
            interactive_oldest_enqueued_ms_plus_one: AtomicU64::new(0),
            maintenance_queued: AtomicUsize::new(0),
            maintenance_oldest_enqueued_ms_plus_one: AtomicU64::new(0),
            interactive_running: AtomicUsize::new(0),
            maintenance_running: AtomicUsize::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        duration_millis_u64(self.origin.elapsed())
    }

    fn record(&self, snapshot: &DispatchLivenessSnapshot) {
        let now_ms = self.now_ms();
        let encode_oldest = |queued: usize, oldest_age_ms: Option<u64>| {
            if queued == 0 {
                0
            } else {
                now_ms
                    .saturating_sub(oldest_age_ms.unwrap_or(0))
                    .saturating_add(1)
            }
        };
        self.interactive_oldest_enqueued_ms_plus_one.store(
            encode_oldest(
                snapshot.interactive.queued,
                snapshot.interactive.oldest_age_ms,
            ),
            Ordering::Relaxed,
        );
        self.maintenance_oldest_enqueued_ms_plus_one.store(
            encode_oldest(
                snapshot.maintenance.queued,
                snapshot.maintenance.oldest_age_ms,
            ),
            Ordering::Relaxed,
        );
        self.interactive_running
            .store(snapshot.running.interactive, Ordering::Release);
        self.maintenance_running
            .store(snapshot.running.maintenance, Ordering::Release);
        self.interactive_queued
            .store(snapshot.interactive.queued, Ordering::Release);
        self.maintenance_queued
            .store(snapshot.maintenance.queued, Ordering::Release);
    }

    fn snapshot(&self, config: &EffectiveConfig) -> DispatchLivenessSnapshot {
        let now_ms = self.now_ms();
        let decode_oldest = |queued: usize, encoded: u64| {
            (queued > 0 && encoded > 0).then(|| now_ms.saturating_sub(encoded - 1))
        };
        let interactive_queued = self.interactive_queued.load(Ordering::Acquire);
        let maintenance_queued = self.maintenance_queued.load(Ordering::Acquire);
        DispatchLivenessSnapshot {
            interactive: DispatchClassQueueSnapshot {
                queued: interactive_queued,
                oldest_age_ms: decode_oldest(
                    interactive_queued,
                    self.interactive_oldest_enqueued_ms_plus_one
                        .load(Ordering::Relaxed),
                ),
            },
            maintenance: DispatchClassQueueSnapshot {
                queued: maintenance_queued,
                oldest_age_ms: decode_oldest(
                    maintenance_queued,
                    self.maintenance_oldest_enqueued_ms_plus_one
                        .load(Ordering::Relaxed),
                ),
            },
            running: DispatchRunningSnapshot {
                interactive: self.interactive_running.load(Ordering::Acquire),
                maintenance: self.maintenance_running.load(Ordering::Acquire),
            },
            interactive_reserve: config.interactive_reserve,
            maintenance_cap: config.maintenance_cap,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutatingLaneSnapshot {
    pub root_id: ProjectRootId,
    pub request_id: String,
    pub command: String,
    pub started_age_ms: u64,
}

/// Non-blocking scheduler explanation attached to a delayed RouteBind warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindBlockerReaderSnapshot {
    pub request_id: String,
    pub command: String,
    pub lane: Lane,
    pub started_age_ms: u64,
    pub execution_started: bool,
    pub started_before_oldest_writer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindBlockerSnapshot {
    pub configure_state: &'static str,
    pub configure_phase_timings: Option<String>,
    pub blockers: Vec<String>,
    pub oldest_queued_writer_age_ms: Option<u64>,
    pub in_flight_readers: Vec<BindBlockerReaderSnapshot>,
    pub reader_admissions_while_promoted_writer_waited: u64,
}

/// Cooperative cancellation for one cancellable executor job.
///
/// One atomic state machine (pending → running → committed | cancelled) is
/// shared by the executor, the canceller, and the job. `cancel` and
/// `try_seal_committed` race through compare-exchange on the same cell, so
/// exactly one wins: a cancel that lands first makes the seal fail (the job
/// must abort before mutating), and a seal that lands first makes the cancel
/// report `RunningCommitted` (the mutation tail finishes normally). A job
/// observes cancellation only at its own checkpoints
/// ([`JobCancellation::cancel_requested_before_commit`]).
#[derive(Debug, Clone)]
pub struct JobCancellation {
    inner: Arc<JobCancellationInner>,
}

#[derive(Debug)]
struct JobCancellationInner {
    state: AtomicU8,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

const JOB_CANCEL_STATE_PENDING: u8 = 0;
const JOB_CANCEL_STATE_RUNNING: u8 = 1;
const JOB_CANCEL_STATE_COMMITTED: u8 = 2;
const JOB_CANCEL_STATE_CANCELLED: u8 = 3;

impl JobCancellation {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(JobCancellationInner {
                state: AtomicU8::new(JOB_CANCEL_STATE_PENDING),
                wait_lock: Mutex::new(()),
                wake: Condvar::new(),
            }),
        }
    }

    fn mark_running(&self) -> bool {
        match self.inner.state.compare_exchange(
            JOB_CANCEL_STATE_PENDING,
            JOB_CANCEL_STATE_RUNNING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(state) => state != JOB_CANCEL_STATE_CANCELLED,
        }
    }

    fn try_cancel_before_running(&self) -> bool {
        let cancelled = self
            .inner
            .state
            .compare_exchange(
                JOB_CANCEL_STATE_PENDING,
                JOB_CANCEL_STATE_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok();
        if cancelled {
            let _wait_guard = self.inner.wait_lock.lock();
            self.inner.wake.notify_all();
        }
        cancelled
    }

    /// Seal the job as committed. Returns `false` when a cancel already won
    /// the race: the job must return early WITHOUT mutating, because the
    /// canceller was told `RunningSignalled` and will discard its completion.
    #[must_use]
    pub fn try_seal_committed(&self) -> bool {
        loop {
            let current = self.state();
            match current {
                JOB_CANCEL_STATE_CANCELLED => return false,
                JOB_CANCEL_STATE_COMMITTED => return true,
                _ => {
                    if self
                        .inner
                        .state
                        .compare_exchange(
                            current,
                            JOB_CANCEL_STATE_COMMITTED,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
        }
    }

    /// Move to cancelled unless the job already sealed its commit. Returns the
    /// state the transition observed (`COMMITTED` when the seal won).
    fn signal_cancel(&self) -> u8 {
        let observed = loop {
            let current = self.state();
            match current {
                JOB_CANCEL_STATE_COMMITTED | JOB_CANCEL_STATE_CANCELLED => break current,
                _ => {
                    if self
                        .inner
                        .state
                        .compare_exchange(
                            current,
                            JOB_CANCEL_STATE_CANCELLED,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        break current;
                    }
                }
            }
        };
        if observed != JOB_CANCEL_STATE_COMMITTED {
            let _wait_guard = self.inner.wait_lock.lock();
            self.inner.wake.notify_all();
        }
        observed
    }

    fn state(&self) -> u8 {
        self.inner.state.load(Ordering::SeqCst)
    }

    /// Request cooperative cancellation without requiring an executor actor.
    /// Detached continuations use this after their setup job has already left
    /// the scheduler's running-job table.
    pub fn request_cancel(&self) {
        self.signal_cancel();
    }

    /// True when a cancel won the state race and the job must abort.
    pub fn cancel_requested_before_commit(&self) -> bool {
        self.state() == JOB_CANCEL_STATE_CANCELLED
    }

    /// Sleep until cancellation is requested or the timeout expires.
    ///
    /// Cancellation takes the same lock before notifying, so a waiter cannot
    /// miss a signal between its state check and the condition-variable wait.
    pub fn wait_for_cancellation(&self, timeout: Duration) -> bool {
        if self.cancel_requested_before_commit() {
            return true;
        }
        let mut guard = self.inner.wait_lock.lock();
        if self.cancel_requested_before_commit() {
            return true;
        }
        self.inner.wake.wait_for(&mut guard, timeout);
        self.cancel_requested_before_commit()
    }

    fn same_token(&self, other: &JobCancellation) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// Outcome of [`Executor::cancel_job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCancelOutcome {
    /// The job had not started; it was removed from the queue and its
    /// completion settled with `request_cancelled`.
    QueuedRemoved,
    /// The job is running; its token is signalled and the job returns through
    /// its normal completion path at the next checkpoint.
    RunningSignalled,
    /// The job already sealed its commit; it finishes normally and the caller
    /// must discard its late completion.
    RunningCommitted,
    /// No queued or tracked job matched the token.
    NotFound,
}

/// Result of cancelling only work whose command closure has not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreExecutionCancelOutcome {
    QueuedRemoved,
    DispatchedCancelled,
    AlreadyStarted,
}

thread_local! {
    static CURRENT_JOB_CANCELLATION: std::cell::RefCell<Option<JobCancellation>> =
        const { std::cell::RefCell::new(None) };
}

/// The cancellation token of the job running on this worker thread, when the
/// job was submitted through [`Executor::submit_cancellable_async`].
pub fn current_job_cancellation() -> Option<JobCancellation> {
    CURRENT_JOB_CANCELLATION.with(|slot| slot.borrow().clone())
}

pub fn current_job_cancelled() -> bool {
    current_job_cancellation().is_some_and(|token| token.cancel_requested_before_commit())
}

pub struct JobCancellationContextGuard {
    previous: Option<JobCancellation>,
}

impl JobCancellationContextGuard {
    fn install(token: Option<JobCancellation>) -> Self {
        let previous = CURRENT_JOB_CANCELLATION.with(|slot| slot.replace(token));
        Self { previous }
    }
}

impl Drop for JobCancellationContextGuard {
    fn drop(&mut self) {
        CURRENT_JOB_CANCELLATION.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Install a cancellable executor job's token on a continuation thread.
///
/// Executor workers install this automatically. A setup job that hands work to
/// a detached thread must install the same token there so existing cooperative
/// checkpoints keep observing route or transport abandonment.
pub fn install_job_cancellation(token: JobCancellation) -> JobCancellationContextGuard {
    JobCancellationContextGuard::install(Some(token))
}

#[derive(Debug, Clone)]
struct RunningJob {
    root_id: ProjectRootId,
    request_id: String,
    command: String,
    job_class: JobClass,
    lane: Lane,
    started_at: Instant,
    execution_started: Arc<AtomicBool>,
    occupancy_reported: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LongRunningInteractiveJobSnapshot {
    pub(crate) root_id: ProjectRootId,
    pub(crate) request_id: String,
    pub(crate) command: String,
    pub(crate) lane: Lane,
    pub(crate) age: Duration,
    pub(crate) execution_started: bool,
}

#[derive(Debug, Clone)]
struct RunningMutatingJob {
    request_id: String,
    command: String,
    started_at: Instant,
}

/// Concurrent scheduler-dispatch executor.
pub struct Executor {
    inner: Arc<ExecutorInner>,
}

impl Executor {
    pub fn new() -> Self {
        Self::with_config(ExecutorConfig::default())
    }

    pub fn with_config(config: ExecutorConfig) -> Self {
        let effective = config.effective();
        let state = Arc::new(Mutex::new(SchedulerState::new(effective.clone())));
        let heavy = Arc::new(HeavySemaphore::new(effective.heavy_permits));
        let nonrunnable_dispatches = Arc::new(AtomicUsize::new(0));
        let completed_interactive = Arc::new(AtomicU64::new(0));
        let completed_maintenance = Arc::new(AtomicU64::new(0));
        let dispatch_liveness = Arc::new(DispatchLivenessAtomics::new());
        let (run_tx, run_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let scheduler_state = Arc::clone(&state);
        let scheduler_heavy = Arc::clone(&heavy);
        let scheduler_violations = Arc::clone(&nonrunnable_dispatches);
        let scheduler_completed_interactive = Arc::clone(&completed_interactive);
        let scheduler_completed_maintenance = Arc::clone(&completed_maintenance);
        let scheduler_dispatch_liveness = Arc::clone(&dispatch_liveness);
        let scheduler_handle = thread::Builder::new()
            .name("aft-executor-scheduler".to_string())
            .spawn(move || {
                scheduler_loop(
                    scheduler_state,
                    scheduler_heavy,
                    run_tx,
                    event_rx,
                    scheduler_violations,
                    scheduler_completed_interactive,
                    scheduler_completed_maintenance,
                    scheduler_dispatch_liveness,
                );
            })
            .expect("spawn AFT executor scheduler");

        let mut worker_handles = Vec::with_capacity(effective.pool_size);
        for worker_id in 0..effective.pool_size {
            let worker_rx = run_rx.clone();
            let worker_events = event_tx.clone();
            // Workers run every tool call, including tree-sitter walks over
            // whatever a URL or file turns out to contain. The standalone loop
            // runs those on the 8 MiB main thread; the 2 MiB default for spawned
            // threads let a deep parse tree abort the whole daemon while the same
            // request succeeded standalone. Match the main thread.
            let handle = thread::Builder::new()
                .name(format!("aft-executor-worker-{worker_id}"))
                .stack_size(EXECUTOR_WORKER_STACK_BYTES)
                .spawn(move || worker_loop(worker_rx, worker_events))
                .expect("spawn AFT executor worker");
            worker_handles.push(handle);
        }

        Self {
            inner: Arc::new(ExecutorInner {
                state,
                event_tx,
                scheduler_handle: Mutex::new(Some(scheduler_handle)),
                worker_handles: Mutex::new(worker_handles),
                config: effective,
                nonrunnable_dispatches,
                completed_interactive,
                completed_maintenance,
                dispatch_liveness,
            }),
        }
    }

    /// Register an actor if one is not already present.
    ///
    /// Existing actors keep their current context and scheduler state; subc
    /// routing reuses them and reconfigures through the Mutating lane
    /// rather than replacing the per-root [`AppContext`]. Returns `true` when a
    /// new actor was inserted.
    pub fn register_actor(&self, root_id: ProjectRootId, ctx: Arc<AppContext>) -> bool {
        let memory_root = root_id.as_path().to_path_buf();
        let inserted = {
            let mut state = self.inner.state.lock();
            if state.actors.contains_key(&root_id) {
                false
            } else {
                state.actor_order.push(root_id.clone());
                state
                    .actors
                    .insert(root_id, ActorState::new(Arc::clone(&ctx)));
                true
            }
        };
        if inserted {
            let app = ctx.app();
            crate::root_cache::register_live_scope(&ctx.storage_dir(), &memory_root);
            app.register_memory_context(memory_root, &ctx);
            app.actor_root_registered();
        }
        self.wake_scheduler();
        inserted
    }

    /// Remove an actor from scheduler state.
    ///
    /// This is intentionally minimal: subc uses it only for a just-created
    /// RouteBind actor whose configure failed before any route was installed, so
    /// there is no in-flight work to quiesce. The removed [`AppContext`] is
    /// dropped after releasing the scheduler lock so watcher/LSP teardown never
    /// runs under that mutex.
    pub fn remove_actor(&self, root_id: &ProjectRootId) {
        let removed = {
            let mut state = self.inner.state.lock();
            state.actor_order.retain(|actor_root| actor_root != root_id);
            state.actors.remove(root_id)
        };
        if let Some(actor) = removed.as_ref() {
            let app = actor.ctx.app();
            crate::root_cache::unregister_live_scope(&actor.ctx.storage_dir(), root_id.as_path());
            app.unregister_memory_context(root_id.as_path(), &actor.ctx);
            app.actor_root_unregistered();
        }
        drop(removed);
        self.wake_scheduler();
    }

    /// Return true only when the actor has no queued or running executor work.
    pub fn actor_is_idle(&self, root_id: &ProjectRootId) -> bool {
        let state = self.inner.state.lock();
        state.actors.get(root_id).is_some_and(ActorState::is_idle)
    }

    /// Non-blocking idle probe for maintenance sweeps. A contended scheduler is
    /// reported separately so root retirement can conservatively wait for the
    /// next sweep without stalling the module loop.
    pub fn try_actor_is_idle(&self, root_id: &ProjectRootId) -> Option<bool> {
        let state = self.inner.state.try_lock()?;
        Some(state.actors.get(root_id).is_some_and(ActorState::is_idle))
    }

    /// Forget an idle actor and drop its root-scoped registries off the scheduler
    /// thread. This is reserved for roots whose project directory no longer
    /// exists; retained existing roots are reused on a later bind.
    pub fn retire_idle_actor_in_background(&self, root_id: &ProjectRootId) -> bool {
        let removed = {
            let mut state = self.inner.state.lock();
            if !state.actors.get(root_id).is_some_and(ActorState::is_idle) {
                return false;
            }
            state.actor_order.retain(|actor_root| actor_root != root_id);
            state.actors.remove(root_id)
        };
        let Some(actor) = removed else {
            return false;
        };
        let app = actor.ctx.app();
        crate::root_cache::unregister_live_scope(&actor.ctx.storage_dir(), root_id.as_path());
        app.unregister_memory_context(root_id.as_path(), &actor.ctx);
        app.actor_root_unregistered();
        std::thread::spawn(move || {
            actor.ctx.teardown_deleted_root();
            drop(actor);
        });
        self.wake_scheduler();
        true
    }

    /// Cancel maintenance jobs that have not started for one retained actor.
    ///
    /// Interactive work and already-running maintenance remain untouched. Each
    /// cancelled job receives a normal completion so its caller can settle
    /// bookkeeping through the same path as an executed job.
    pub fn cancel_queued_maintenance(&self, root_id: &ProjectRootId) -> usize {
        let cancelled = {
            let mut state = self.inner.state.lock();
            state
                .actors
                .get_mut(root_id)
                .map(|actor| actor.maintenance.cancel_queued_jobs())
                .unwrap_or(0)
        };
        if cancelled > 0 {
            self.wake_scheduler();
        }
        cancelled
    }

    /// Return whether scheduler state currently has an actor for this root.
    pub fn actor_registered(&self, root_id: &ProjectRootId) -> bool {
        let state = self.inner.state.lock();
        state.actors.contains_key(root_id)
    }

    /// Snapshot one actor context without retaining the scheduler lock while
    /// maintenance drops root-scoped resources.
    pub fn actor_context(&self, root_id: &ProjectRootId) -> Option<Arc<AppContext>> {
        let state = self.inner.state.lock();
        state
            .actors
            .get(root_id)
            .map(|actor| Arc::clone(&actor.ctx))
    }

    /// Snapshot the registered actor contexts.
    ///
    /// The returned [`Arc`]s keep contexts alive after the scheduler lock is
    /// released, so callers can run teardown without holding executor state.
    pub fn actor_contexts(&self) -> Vec<Arc<AppContext>> {
        let state = self.inner.state.lock();
        state
            .actors
            .values()
            .map(|actor_state| Arc::clone(&actor_state.ctx))
            .collect()
    }

    /// Snapshot the registered root ids paired with their actor contexts.
    pub fn actor_entries(&self) -> Vec<(ProjectRootId, Arc<AppContext>)> {
        let state = self.inner.state.lock();
        state
            .actors
            .iter()
            .map(|(root_id, actor_state)| (root_id.clone(), Arc::clone(&actor_state.ctx)))
            .collect()
    }

    /// Non-blocking variant for the health path: the probe reply must stay
    /// cheap under any load, so it skips the actor list (reported as busy)
    /// rather than waiting on the scheduler state lock.
    pub fn try_actor_entries(&self) -> Option<Vec<(ProjectRootId, Arc<AppContext>)>> {
        let state = self.inner.state.try_lock()?;
        Some(
            state
                .actors
                .iter()
                .map(|(root_id, actor_state)| (root_id.clone(), Arc::clone(&actor_state.ctx)))
                .collect(),
        )
    }

    /// Registered actor count under the scheduler lock. Tests that assert a
    /// bind had no actor side effect use this rather than the try-lock form,
    /// which reports contention (`None`) instead of a count.
    pub fn actor_count(&self) -> usize {
        self.inner.state.lock().actors.len()
    }

    /// Constant-time scheduler contention signal for the health reply path.
    pub fn try_actor_count(&self) -> Option<usize> {
        self.inner.state.try_lock().map(|state| state.actors.len())
    }

    pub fn submit(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
    ) -> CompletionHandle {
        let (completion_tx, completion_rx) = crossbeam_channel::bounded(1);
        self.submit_with_completion(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            job,
            CompletionSender::Sync(completion_tx),
        );
        CompletionHandle { rx: completion_rx }
    }

    pub fn submit_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
    ) -> oneshot::Receiver<Response> {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
        );
        completion_rx
    }

    /// Submit an interactive job with an exact-job cancellation token.
    ///
    /// The returned token cancels THIS job only (queued: removed and settled
    /// with `request_cancelled`; running: signalled cooperatively). The job
    /// observes the token via [`current_job_cancellation`].
    pub fn submit_cancellable_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
    ) -> (oneshot::Receiver<Response>, JobCancellation) {
        let cancellation = JobCancellation::new();
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion_cancellable(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
            Some(cancellation.clone()),
            None,
        );
        (completion_rx, cancellation)
    }

    /// Cancel one exact job by its token.
    ///
    /// Queued jobs are removed and settled immediately; running jobs are
    /// signalled and return through their normal completion path at the next
    /// cooperative checkpoint. Jobs that sealed their commit finish normally.
    pub fn cancel_job(&self, root_id: &ProjectRootId, token: &JobCancellation) -> JobCancelOutcome {
        // Signal BEFORE actor lookup, deliberately: a job whose actor was
        // already torn down (fatal teardown, root removal) must still abort at
        // its next checkpoint, so the token is cancelled regardless. The
        // outcome must then reflect what the signal actually did — reporting
        // NotFound for a token that was RUNNING at signal time would mislead
        // the caller into treating a signalled job as nonexistent.
        let observed = token.signal_cancel();
        let (outcome, settled) = {
            let mut state = self.inner.state.lock();
            let Some(actor) = state.actors.get_mut(root_id) else {
                return match observed {
                    JOB_CANCEL_STATE_COMMITTED => JobCancelOutcome::RunningCommitted,
                    JOB_CANCEL_STATE_RUNNING | JOB_CANCEL_STATE_PENDING => {
                        JobCancelOutcome::RunningSignalled
                    }
                    _ => JobCancelOutcome::NotFound,
                };
            };
            match actor.remove_queued_cancellable(token) {
                Some(queued) => (JobCancelOutcome::QueuedRemoved, Some(queued)),
                None => match observed {
                    // The seal won the race: the job commits and finishes.
                    JOB_CANCEL_STATE_COMMITTED => (JobCancelOutcome::RunningCommitted, None),
                    // RUNNING at signal time, or PENDING at signal time but
                    // dispatched before we took the scheduler lock: either way
                    // the job aborts at its next checkpoint.
                    JOB_CANCEL_STATE_RUNNING | JOB_CANCEL_STATE_PENDING => {
                        (JobCancelOutcome::RunningSignalled, None)
                    }
                    // Already cancelled by an earlier call and no longer queued.
                    _ => (JobCancelOutcome::NotFound, None),
                },
            }
        };
        if let Some(queued) = settled {
            queued.completion.send(Response::error(
                queued.request_id,
                "request_cancelled",
                "request cancelled before execution",
            ));
            self.wake_scheduler();
        }
        outcome
    }

    /// Cancel a route job only while its command closure has not started.
    ///
    /// Detaching a route must discard work that cannot produce a live reply,
    /// while a command already executing remains eligible for session replay.
    /// A dispatched job may already reserve scheduler capacity; its worker sees
    /// the cancelled token before invoking the closure and releases that
    /// reservation through the normal completion event.
    pub(crate) fn cancel_job_before_execution(
        &self,
        root_id: &ProjectRootId,
        token: &JobCancellation,
    ) -> PreExecutionCancelOutcome {
        let (outcome, settled) = {
            let mut state = self.inner.state.lock();
            if !token.try_cancel_before_running() {
                return PreExecutionCancelOutcome::AlreadyStarted;
            }
            let settled = state
                .actors
                .get_mut(root_id)
                .and_then(|actor| actor.remove_queued_cancellable(token));
            let outcome = if settled.is_some() {
                PreExecutionCancelOutcome::QueuedRemoved
            } else {
                PreExecutionCancelOutcome::DispatchedCancelled
            };
            (outcome, settled)
        };
        if let Some(queued) = settled {
            queued.completion.send(Response::error(
                queued.request_id,
                "request_cancelled",
                "request cancelled before execution",
            ));
            self.wake_scheduler();
        }
        outcome
    }

    pub fn submit_maintenance_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
    ) -> oneshot::Receiver<Response> {
        self.submit_maintenance_async_with_key(root_id, lane, request_id, job, None)
    }

    pub(crate) fn submit_coalescable_maintenance_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        coalesce_key: MaintenanceCoalesceKey,
        job: ExecutorJob,
    ) -> oneshot::Receiver<Response> {
        self.submit_maintenance_async_with_key(root_id, lane, request_id, job, Some(coalesce_key))
    }

    fn submit_maintenance_async_with_key(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
        coalesce_key: Option<MaintenanceCoalesceKey>,
    ) -> oneshot::Receiver<Response> {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion_cancellable(
            root_id,
            JobClass::Maintenance,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
            None,
            coalesce_key,
        );
        completion_rx
    }

    fn submit_with_completion(
        &self,
        root_id: ProjectRootId,
        job_class: JobClass,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
        completion: CompletionSender,
    ) {
        self.submit_with_completion_cancellable(
            root_id, job_class, lane, request_id, job, completion, None, None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_with_completion_cancellable(
        &self,
        root_id: ProjectRootId,
        job_class: JobClass,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
        completion: CompletionSender,
        cancellation: Option<JobCancellation>,
        maintenance_coalesce_key: Option<MaintenanceCoalesceKey>,
    ) {
        let command = job_command(job_class, lane);
        let mut job = Some(job);
        let mut completion = Some(completion);
        let mut duplicate_victims = Vec::new();

        let response = {
            let mut state = self.inner.state.lock();
            match state.actors.get_mut(&root_id) {
                Some(actor) if actor.fatal => Some(actor_fatal_response(request_id.clone())),
                Some(actor) => {
                    let mut admission_error = None;
                    if job_class == JobClass::Maintenance {
                        if maintenance_coalesce_key
                            .is_some_and(|key| actor.maintenance.has_maintenance_coalesce_key(key))
                        {
                            admission_error = Some(maintenance_cancelled_response(
                                request_id.clone(),
                                "maintenance drain coalesced behind an identical queued drain",
                            ));
                        } else {
                            if actor.maintenance.queued_count() >= MAINTENANCE_QUEUE_CAP {
                                duplicate_victims =
                                    actor.maintenance.remove_duplicate_maintenance_jobs();
                            }
                            if actor.maintenance.queued_count() >= MAINTENANCE_QUEUE_CAP {
                                admission_error =
                                    Some(maintenance_backpressure_response(request_id.clone()));
                            }
                        }
                    }

                    if admission_error.is_none() {
                        actor.push_job(
                            job_class,
                            lane,
                            QueuedJob {
                                job: job.take().expect("executor job already queued"),
                                completion: completion
                                    .take()
                                    .expect("executor completion already queued"),
                                request_id: request_id.clone(),
                                command,
                                queued_at: Instant::now(),
                                cancellation: cancellation.clone(),
                                maintenance_coalesce_key,
                            },
                        );
                    }
                    admission_error
                }
                None => Some(Response::error(
                    request_id.clone(),
                    "actor_not_registered",
                    "executor actor is not registered",
                )),
            }
        };

        for victim in duplicate_victims {
            victim.completion.send(maintenance_cancelled_response(
                victim.request_id,
                "duplicate maintenance drain removed to preserve queue capacity",
            ));
        }

        if let Some(response) = response {
            if let Some(completion) = completion {
                completion.send(response);
            }
            return;
        }

        self.wake_scheduler();
    }

    pub fn pool_size(&self) -> usize {
        self.inner.config.pool_size
    }

    pub fn actor_cap(&self) -> usize {
        self.inner.config.actor_cap
    }

    pub fn read_cap(&self) -> usize {
        self.inner.config.read_cap
    }

    pub fn heavy_permits(&self) -> usize {
        self.inner.config.heavy_permits
    }

    pub fn interactive_reserve(&self) -> usize {
        self.inner.config.interactive_reserve
    }

    pub fn maintenance_cap(&self) -> usize {
        self.inner.config.maintenance_cap
    }

    pub fn try_dispatch_liveness_snapshot(&self) -> Option<DispatchLivenessSnapshot> {
        Some(self.inner.dispatch_liveness.snapshot(&self.inner.config))
    }

    pub fn try_mutating_lane_snapshots(&self) -> Option<Vec<MutatingLaneSnapshot>> {
        self.inner
            .state
            .try_lock()
            .map(|state| state.mutating_lane_snapshots())
    }

    pub fn try_mutating_job_state_label(
        &self,
        root_id: &ProjectRootId,
        request_id: &str,
    ) -> Option<&'static str> {
        self.inner
            .state
            .try_lock()
            .map(|state| state.mutating_job_state_label(root_id, request_id))
    }

    /// Snapshot RouteBind blockers without waiting on scheduler state. The subc
    /// health path uses this only for a delayed-bind breadcrumb, so contention
    /// is reported as scheduler busy rather than delaying the transport loop.
    pub fn try_bind_blocker_snapshot(
        &self,
        root_id: &ProjectRootId,
        request_id: &str,
    ) -> Option<BindBlockerSnapshot> {
        self.inner
            .state
            .try_lock()
            .map(|state| state.bind_blocker_snapshot(root_id, request_id))
    }

    /// Return each over-age interactive occupant once. Marking happens under the
    /// scheduler lock so repeated transport-loop censuses cannot flood logs while
    /// the same job remains stuck.
    pub(crate) fn try_take_long_running_interactive_jobs(
        &self,
        minimum_age: Duration,
    ) -> Option<Vec<LongRunningInteractiveJobSnapshot>> {
        let now = Instant::now();
        let mut state = self.inner.state.try_lock()?;
        let mut snapshots = state
            .running_jobs
            .values_mut()
            .filter_map(|job| {
                let age = now.saturating_duration_since(job.started_at);
                if job.job_class != JobClass::Interactive
                    || job.occupancy_reported
                    || age < minimum_age
                {
                    return None;
                }
                job.occupancy_reported = true;
                Some(LongRunningInteractiveJobSnapshot {
                    root_id: job.root_id.clone(),
                    request_id: job.request_id.clone(),
                    command: job.command.clone(),
                    lane: job.lane,
                    age,
                    execution_started: job.execution_started.load(Ordering::Acquire),
                })
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.root_id
                .as_path()
                .cmp(right.root_id.as_path())
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        Some(snapshots)
    }

    pub fn nonrunnable_dispatch_count(&self) -> usize {
        self.inner.nonrunnable_dispatches.load(Ordering::Acquire)
    }

    pub fn completion_counts(&self) -> (u64, u64) {
        (
            self.inner.completed_interactive.load(Ordering::Relaxed),
            self.inner.completed_maintenance.load(Ordering::Relaxed),
        )
    }

    pub fn actor_is_fatal(&self, root_id: &ProjectRootId) -> bool {
        self.inner
            .state
            .lock()
            .actors
            .get(root_id)
            .map(|actor| actor.fatal)
            .unwrap_or(false)
    }

    fn wake_scheduler(&self) {
        let _ = self.inner.event_tx.send(SchedulerEvent::Wake);
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

struct ExecutorInner {
    state: Arc<Mutex<SchedulerState>>,
    event_tx: Sender<SchedulerEvent>,
    scheduler_handle: Mutex<Option<JoinHandle<()>>>,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
    config: EffectiveConfig,
    nonrunnable_dispatches: Arc<AtomicUsize>,
    completed_interactive: Arc<AtomicU64>,
    completed_maintenance: Arc<AtomicU64>,
    dispatch_liveness: Arc<DispatchLivenessAtomics>,
}

impl Drop for ExecutorInner {
    fn drop(&mut self) {
        let _ = self.event_tx.send(SchedulerEvent::Shutdown);

        if let Some(handle) = self.scheduler_handle.lock().take() {
            let _ = handle.join();
        }

        let mut workers = self.worker_handles.lock();
        for handle in workers.drain(..) {
            let _ = handle.join();
        }
    }
}

struct SchedulerState {
    actors: HashMap<ProjectRootId, ActorState>,
    actor_order: Vec<ProjectRootId>,
    cursor: usize,
    idle_workers: usize,
    interactive_inflight: usize,
    maintenance_inflight: usize,
    config: EffectiveConfig,
    running_jobs: HashMap<(ProjectRootId, String), RunningJob>,
}

impl SchedulerState {
    fn new(config: EffectiveConfig) -> Self {
        Self {
            actors: HashMap::new(),
            actor_order: Vec::new(),
            cursor: 0,
            idle_workers: config.pool_size,
            interactive_inflight: 0,
            maintenance_inflight: 0,
            config,
            running_jobs: HashMap::new(),
        }
    }

    fn dispatch_liveness_snapshot(&self) -> DispatchLivenessSnapshot {
        let now = Instant::now();
        let mut interactive = QueueSnapshotAccumulator::default();
        let mut maintenance = QueueSnapshotAccumulator::default();
        for actor in self.actors.values() {
            interactive.add(actor.class_queues(JobClass::Interactive), now);
            maintenance.add(actor.class_queues(JobClass::Maintenance), now);
        }

        DispatchLivenessSnapshot {
            interactive: interactive.finish(),
            maintenance: maintenance.finish(),
            running: DispatchRunningSnapshot {
                interactive: self.interactive_inflight,
                maintenance: self.maintenance_inflight,
            },
            interactive_reserve: self.config.interactive_reserve,
            maintenance_cap: self.config.maintenance_cap,
        }
    }

    fn mutating_lane_snapshots(&self) -> Vec<MutatingLaneSnapshot> {
        let now = Instant::now();
        let mut snapshots: Vec<_> = self
            .actors
            .iter()
            .filter_map(|(root_id, actor)| {
                actor
                    .mutating_inflight
                    .as_ref()
                    .map(|job| MutatingLaneSnapshot {
                        root_id: root_id.clone(),
                        request_id: job.request_id.clone(),
                        command: job.command.clone(),
                        started_age_ms: duration_millis_u64(
                            now.saturating_duration_since(job.started_at),
                        ),
                    })
            })
            .collect();
        snapshots.sort_by(|left, right| left.root_id.as_path().cmp(right.root_id.as_path()));
        snapshots
    }

    fn mutating_job_state_label(&self, root_id: &ProjectRootId, request_id: &str) -> &'static str {
        let Some(actor) = self.actors.get(root_id) else {
            return "actor_missing";
        };
        if actor
            .mutating_inflight
            .as_ref()
            .is_some_and(|job| job.request_id == request_id)
        {
            return "running";
        }
        if actor.has_queued_mutating_job(request_id) {
            return "queued";
        }
        if actor.writer_inflight {
            return "blocked_by_other_mutating";
        }
        "not_found"
    }

    fn bind_blocker_snapshot(
        &self,
        root_id: &ProjectRootId,
        request_id: &str,
    ) -> BindBlockerSnapshot {
        let configure_state = self.mutating_job_state_label(root_id, request_id);
        let now = Instant::now();
        let actor = self.actors.get(root_id);
        let oldest_queued_writer_at = actor.and_then(ActorState::oldest_queued_writer_at);
        let oldest_queued_writer_age_ms = oldest_queued_writer_at
            .map(|queued_at| duration_millis_u64(now.saturating_duration_since(queued_at)));
        let mut in_flight_readers = self
            .running_jobs
            .values()
            .filter(|job| {
                job.root_id == *root_id
                    && matches!(job.lane, Lane::PureRead | Lane::SerialLspStatus)
            })
            .map(|job| BindBlockerReaderSnapshot {
                request_id: job.request_id.clone(),
                command: job.command.clone(),
                lane: job.lane,
                started_age_ms: duration_millis_u64(now.saturating_duration_since(job.started_at)),
                execution_started: job.execution_started.load(Ordering::Acquire),
                started_before_oldest_writer: oldest_queued_writer_at
                    .is_some_and(|queued_at| job.started_at <= queued_at),
            })
            .collect::<Vec<_>>();
        in_flight_readers.sort_by(|left, right| left.request_id.cmp(&right.request_id));

        let mut blockers = Vec::new();
        if let Some(actor) = actor {
            if configure_state == "queued" {
                let configure_count = actor.pending_configure_count();
                if configure_count > 0 {
                    blockers.push(format!("queued_behind_configure({configure_count})"));
                }
                if actor.read_inflight > 0 || actor.lsp_inflight {
                    let zombie_readers = in_flight_readers
                        .iter()
                        .filter(|reader| !reader.execution_started)
                        .map(format_bind_blocker_reader)
                        .collect::<Vec<_>>();
                    let stuck_readers = in_flight_readers
                        .iter()
                        .filter(|reader| {
                            reader.execution_started
                                && reader.started_age_ms
                                    >= duration_millis_u64(READER_STUCK_CENSUS_AGE)
                        })
                        .map(format_bind_blocker_reader)
                        .collect::<Vec<_>>();
                    if !zombie_readers.is_empty() {
                        blockers.push(format!("zombie_reader({})", zombie_readers.join("; ")));
                    } else if !stuck_readers.is_empty() {
                        blockers.push(format!(
                            "waiting_on_readers_stuck({})",
                            stuck_readers.join("; ")
                        ));
                    } else {
                        blockers.push("waiting_on_readers".to_string());
                    }
                }
            }
        }

        if configure_state == "queued" {
            let maintenance: Vec<_> = self
                .running_jobs
                .values()
                .filter(|job| job.job_class == JobClass::Maintenance)
                .collect();
            if !maintenance.is_empty() {
                blockers.push(format!(
                    "queued_behind_maintenance({})",
                    format_running_jobs(&maintenance)
                ));
            }
        }

        if self.idle_workers == 0 {
            let running: Vec<_> = self.running_jobs.values().collect();
            blockers.push(format!(
                "idle_workers==0({})",
                format_running_jobs(&running)
            ));
        }

        BindBlockerSnapshot {
            configure_state,
            configure_phase_timings: actor.map(|actor| actor.ctx.configure_ack_phase_snapshot()),
            blockers,
            oldest_queued_writer_age_ms,
            in_flight_readers,
            reader_admissions_while_promoted_writer_waited: actor
                .map(|actor| actor.reader_admissions_while_promoted_writer_waited)
                .unwrap_or(0),
        }
    }
}

fn is_configure_request(request_id: &str) -> bool {
    request_id.starts_with("subc-bind-")
}

fn format_bind_blocker_reader(reader: &BindBlockerReaderSnapshot) -> String {
    format!(
        "job={} command={} lane={:?} age_ms={} execution_started={} started_before_oldest_writer={}",
        reader.request_id,
        reader.command,
        reader.lane,
        reader.started_age_ms,
        reader.execution_started,
        reader.started_before_oldest_writer
    )
}

fn format_running_jobs(jobs: &[&RunningJob]) -> String {
    let now = Instant::now();
    let mut labels: Vec<_> = jobs
        .iter()
        .map(|job| {
            format!(
                "job={} command={} lane={:?} root={} age_ms={}",
                job.request_id,
                job.command,
                job.lane,
                job.root_id.as_path().display(),
                duration_millis_u64(now.saturating_duration_since(job.started_at))
            )
        })
        .collect();
    labels.sort();
    labels.truncate(4);
    labels.join("; ")
}

#[derive(Default)]
struct QueueSnapshotAccumulator {
    queued: usize,
    oldest_age_ms: Option<u64>,
}

impl QueueSnapshotAccumulator {
    fn add(&mut self, queues: &ClassQueues, now: Instant) {
        self.queued += queues.queued_count();
        if let Some(queued_at) = queues.oldest_queued_at() {
            let age_ms = duration_millis_u64(now.saturating_duration_since(queued_at));
            self.oldest_age_ms = Some(
                self.oldest_age_ms
                    .map_or(age_ms, |oldest| oldest.max(age_ms)),
            );
        }
    }

    fn finish(self) -> DispatchClassQueueSnapshot {
        DispatchClassQueueSnapshot {
            queued: self.queued,
            oldest_age_ms: self.oldest_age_ms,
        }
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

struct ActorState {
    ctx: Arc<AppContext>,
    epoch: Arc<RwLock<()>>,
    read_inflight: usize,
    lsp_inflight: bool,
    actor_total_inflight: usize,
    writer_inflight: bool,
    maintenance_commit_inflight: bool,
    mutating_inflight: Option<RunningMutatingJob>,
    reader_admissions_while_promoted_writer_waited: u64,
    deficit: isize,
    interactive: ClassQueues,
    maintenance: ClassQueues,
    fatal: bool,
}

impl ActorState {
    fn new(ctx: Arc<AppContext>) -> Self {
        Self {
            ctx,
            epoch: Arc::new(RwLock::new(())),
            read_inflight: 0,
            lsp_inflight: false,
            actor_total_inflight: 0,
            writer_inflight: false,
            maintenance_commit_inflight: false,
            mutating_inflight: None,
            reader_admissions_while_promoted_writer_waited: 0,
            deficit: 0,
            interactive: ClassQueues::new(),
            maintenance: ClassQueues::new(),
            fatal: false,
        }
    }

    fn push_job(&mut self, job_class: JobClass, lane: Lane, job: QueuedJob) {
        self.class_queues_mut(job_class).push_job(lane, job);
    }

    fn has_queued_jobs(&self) -> bool {
        self.interactive.has_queued_jobs() || self.maintenance.has_queued_jobs()
    }

    fn is_idle(&self) -> bool {
        self.actor_total_inflight == 0 && !self.has_queued_jobs()
    }

    fn has_queued_jobs_for(&self, job_class: JobClass) -> bool {
        self.class_queues(job_class).has_queued_jobs()
    }

    fn front_lane(&self, job_class: JobClass) -> Option<Lane> {
        self.class_queues(job_class).front_lane()
    }

    fn pop_front_job(&mut self, job_class: JobClass, lane: Lane) -> Option<QueuedJob> {
        self.class_queues_mut(job_class).pop_front_job(lane)
    }

    fn higher_priority_writer_barrier_blocks(&self, job_class: JobClass) -> bool {
        // Maintenance must not start while interactive mutating work (tool
        // mutations, route binds) waits: a maintenance job that takes the
        // actor's writer slot would push the interactive writer behind it.
        matches!(job_class, JobClass::Maintenance)
            && !self.interactive.queue(Lane::Mutating).is_empty()
    }

    fn class_queues(&self, job_class: JobClass) -> &ClassQueues {
        match job_class {
            JobClass::Interactive => &self.interactive,
            JobClass::Maintenance => &self.maintenance,
        }
    }

    fn class_queues_mut(&mut self, job_class: JobClass) -> &mut ClassQueues {
        match job_class {
            JobClass::Interactive => &mut self.interactive,
            JobClass::Maintenance => &mut self.maintenance,
        }
    }

    fn fail_queued_jobs(&mut self) {
        self.interactive.fail_queued_jobs();
        self.maintenance.fail_queued_jobs();
    }

    fn has_queued_mutating_job(&self, request_id: &str) -> bool {
        self.interactive.has_queued_mutating_job(request_id)
            || self.maintenance.has_queued_mutating_job(request_id)
    }

    fn remove_queued_cancellable(&mut self, token: &JobCancellation) -> Option<QueuedJob> {
        self.interactive
            .remove_cancellable(token)
            .or_else(|| self.maintenance.remove_cancellable(token))
    }

    fn oldest_queued_writer_at(&self) -> Option<Instant> {
        [
            self.interactive.queue(Lane::Mutating).front(),
            self.maintenance.queue(Lane::Mutating).front(),
        ]
        .into_iter()
        .flatten()
        .map(|job| job.queued_at)
        .min()
    }

    fn pending_configure_count(&self) -> usize {
        usize::from(
            self.mutating_inflight
                .as_ref()
                .is_some_and(|job| is_configure_request(&job.request_id)),
        ) + self.interactive.queued_configure_count()
            + self.maintenance.queued_configure_count()
    }
}

struct ClassQueues {
    order: VecDeque<Lane>,
    pure_reads: VecDeque<QueuedJob>,
    lsp_status: VecDeque<QueuedJob>,
    heavy_init: VecDeque<QueuedJob>,
    mutating: VecDeque<QueuedJob>,
    maintenance_commit: VecDeque<QueuedJob>,
}

impl ClassQueues {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            pure_reads: VecDeque::new(),
            lsp_status: VecDeque::new(),
            heavy_init: VecDeque::new(),
            mutating: VecDeque::new(),
            maintenance_commit: VecDeque::new(),
        }
    }

    fn push_job(&mut self, lane: Lane, job: QueuedJob) {
        self.order.push_back(lane);
        self.queue_mut(lane).push_back(job);
    }

    fn has_queued_jobs(&self) -> bool {
        !self.order.is_empty()
    }

    fn front_lane(&self) -> Option<Lane> {
        self.order.front().copied()
    }

    /// Interactive admission order: a hard-starved configure (queued RouteBind
    /// older than the promotion age) preempts everything so its daemon deadline
    /// survives; otherwise pure reads go first (they overlap each other and
    /// never barrier the actor), then remaining lanes in arrival order.
    /// Maintenance keeps strict arrival order via `front_lane`.
    fn next_interactive_lane(&self, now: Instant) -> Option<Lane> {
        let starved_writer = self.mutating.iter().any(|job| {
            now.saturating_duration_since(job.queued_at) >= INTERACTIVE_WRITER_PROMOTION_AGE
        });
        if starved_writer {
            // Also stops NEW readers from being admitted on this actor while
            // the promoted writer waits for in-flight readers to drain.
            return Some(Lane::Mutating);
        }
        if !self.pure_reads.is_empty() {
            return Some(Lane::PureRead);
        }
        self.order
            .iter()
            .copied()
            .find(|lane| *lane != Lane::PureRead)
    }

    fn pop_front_job(&mut self, lane: Lane) -> Option<QueuedJob> {
        // Keep `order` consistent with per-lane queues when admission picks a
        // lane other than the arrival-order head: remove the FIRST occurrence
        // of the chosen lane from `order`, not necessarily the front.
        let position = self.order.iter().position(|queued| *queued == lane)?;
        self.order.remove(position);
        self.queue_mut(lane).pop_front()
    }

    fn queued_count(&self) -> usize {
        self.order.len()
    }

    fn has_maintenance_coalesce_key(&self, key: MaintenanceCoalesceKey) -> bool {
        [
            Lane::PureRead,
            Lane::SerialLspStatus,
            Lane::HeavyInit,
            Lane::Mutating,
            Lane::MaintenanceCommit,
        ]
        .into_iter()
        .any(|lane| {
            self.queue(lane)
                .iter()
                .any(|job| job.maintenance_coalesce_key == Some(key))
        })
    }

    /// Remove only redundant idempotent drains while preserving survivor order.
    fn remove_duplicate_maintenance_jobs(&mut self) -> Vec<QueuedJob> {
        let mut seen = HashSet::new();
        let mut lane_positions = [0usize; 5];
        let mut duplicates = Vec::new();

        for (order_position, lane) in self.order.iter().copied().enumerate() {
            let lane_index = lane_index(lane);
            let queue_position = lane_positions[lane_index];
            lane_positions[lane_index] += 1;
            let Some(key) = self
                .queue(lane)
                .get(queue_position)
                .and_then(|job| job.maintenance_coalesce_key)
            else {
                continue;
            };
            if !seen.insert(key) {
                duplicates.push((order_position, lane, queue_position));
            }
        }

        let mut removed = Vec::with_capacity(duplicates.len());
        for (order_position, lane, queue_position) in duplicates.into_iter().rev() {
            self.order.remove(order_position);
            if let Some(job) = self.queue_mut(lane).remove(queue_position) {
                removed.push(job);
            }
        }
        removed
    }

    fn oldest_queued_at(&self) -> Option<Instant> {
        self.front_lane()
            .and_then(|lane| self.queue(lane).front().map(|job| job.queued_at))
    }

    fn fail_queued_jobs(&mut self) {
        self.order.clear();
        fail_queued_job_queue(&mut self.pure_reads);
        fail_queued_job_queue(&mut self.lsp_status);
        fail_queued_job_queue(&mut self.heavy_init);
        fail_queued_job_queue(&mut self.mutating);
        fail_queued_job_queue(&mut self.maintenance_commit);
    }

    fn cancel_queued_jobs(&mut self) -> usize {
        self.order.clear();
        cancel_queued_job_queue(&mut self.pure_reads)
            + cancel_queued_job_queue(&mut self.lsp_status)
            + cancel_queued_job_queue(&mut self.heavy_init)
            + cancel_queued_job_queue(&mut self.mutating)
            + cancel_queued_job_queue(&mut self.maintenance_commit)
    }

    fn has_queued_mutating_job(&self, request_id: &str) -> bool {
        self.mutating.iter().any(|job| job.request_id == request_id)
    }

    /// Remove the queued job carrying this exact cancellation token.
    ///
    /// `order` holds one lane entry per push, and per-lane entries pair FIFO
    /// with the lane queue: the k-th occurrence of a lane in `order`
    /// corresponds to the k-th element of that lane's queue. Removing the
    /// FIRST matching order entry for a job deeper in its lane queue would
    /// shift the pairing and reorder the survivors, so the occurrence at the
    /// job's own queue position is removed instead.
    fn remove_cancellable(&mut self, token: &JobCancellation) -> Option<QueuedJob> {
        for lane in [
            Lane::PureRead,
            Lane::SerialLspStatus,
            Lane::HeavyInit,
            Lane::Mutating,
            Lane::MaintenanceCommit,
        ] {
            let queue = self.queue_mut(lane);
            let position = queue.iter().position(|queued| {
                queued
                    .cancellation
                    .as_ref()
                    .is_some_and(|candidate| candidate.same_token(token))
            });
            if let Some(position) = position {
                let removed = queue.remove(position);
                let mut occurrence = 0usize;
                if let Some(order_position) = self.order.iter().position(|entry| {
                    if *entry != lane {
                        return false;
                    }
                    let matched = occurrence == position;
                    occurrence += 1;
                    matched
                }) {
                    self.order.remove(order_position);
                }
                return removed;
            }
        }
        None
    }

    fn queued_configure_count(&self) -> usize {
        self.mutating
            .iter()
            .filter(|job| is_configure_request(&job.request_id))
            .count()
    }

    fn queue(&self, lane: Lane) -> &VecDeque<QueuedJob> {
        match lane {
            Lane::PureRead => &self.pure_reads,
            Lane::SerialLspStatus => &self.lsp_status,
            Lane::HeavyInit => &self.heavy_init,
            Lane::Mutating => &self.mutating,
            Lane::MaintenanceCommit => &self.maintenance_commit,
        }
    }

    fn queue_mut(&mut self, lane: Lane) -> &mut VecDeque<QueuedJob> {
        match lane {
            Lane::PureRead => &mut self.pure_reads,
            Lane::SerialLspStatus => &mut self.lsp_status,
            Lane::HeavyInit => &mut self.heavy_init,
            Lane::Mutating => &mut self.mutating,
            Lane::MaintenanceCommit => &mut self.maintenance_commit,
        }
    }
}

struct QueuedJob {
    job: ExecutorJob,
    completion: CompletionSender,
    request_id: String,
    command: String,
    queued_at: Instant,
    cancellation: Option<JobCancellation>,
    maintenance_coalesce_key: Option<MaintenanceCoalesceKey>,
}

fn lane_index(lane: Lane) -> usize {
    match lane {
        Lane::PureRead => 0,
        Lane::SerialLspStatus => 1,
        Lane::HeavyInit => 2,
        Lane::Mutating => 3,
        Lane::MaintenanceCommit => 4,
    }
}

fn fail_queued_job_queue(queue: &mut VecDeque<QueuedJob>) {
    for queued in queue.drain(..) {
        queued
            .completion
            .send(actor_fatal_response(queued.request_id));
    }
}

fn cancel_queued_job_queue(queue: &mut VecDeque<QueuedJob>) -> usize {
    let cancelled = queue.len();
    for queued in queue.drain(..) {
        queued.completion.send(Response::error(
            queued.request_id,
            "maintenance_cancelled",
            "maintenance cancelled because the actor has no bound routes",
        ));
    }
    cancelled
}

fn job_command(job_class: JobClass, lane: Lane) -> String {
    format!("executor::{job_class:?}::{lane:?}")
}

fn maintenance_cancelled_response(
    request_id: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    Response::error(request_id, "maintenance_cancelled", message)
}

fn maintenance_backpressure_response(request_id: impl Into<String>) -> Response {
    Response::error_with_data(
        request_id,
        "maintenance_backpressure",
        format!("maintenance queue reached its per-actor capacity of {MAINTENANCE_QUEUE_CAP} jobs"),
        serde_json::json!({
            "retryable": true,
            "queue_cap": MAINTENANCE_QUEUE_CAP,
        }),
    )
}

fn actor_fatal_response(request_id: impl Into<String>) -> Response {
    Response::error(
        request_id,
        "actor_fatal",
        "executor actor is fatal after a mutating job panic",
    )
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn panic_response(
    request_id: impl Into<String>,
    command: &str,
    payload: &(dyn std::any::Any + Send),
) -> Response {
    let panic_message = panic_payload_message(payload);
    Response::error(
        request_id,
        "actor_fatal",
        format!("command '{command}' panicked: {panic_message}"),
    )
}

enum CompletionSender {
    Sync(Sender<Response>),
    Async(oneshot::Sender<Response>),
}

impl CompletionSender {
    fn send(self, response: Response) {
        match self {
            Self::Sync(tx) => {
                let _ = tx.send(response);
            }
            Self::Async(tx) => {
                let _ = tx.send(response);
            }
        }
    }
}

struct RunJob {
    root_id: ProjectRootId,
    job_class: JobClass,
    lane: Lane,
    ctx: Arc<AppContext>,
    epoch: Arc<RwLock<()>>,
    job: ExecutorJob,
    completion: Option<CompletionSender>,
    request_id: String,
    command: String,
    heavy_permit: Option<HeavyPermit>,
    cancellation: Option<JobCancellation>,
    execution_started: Arc<AtomicBool>,
}

struct CompletionEvent {
    root_id: ProjectRootId,
    request_id: String,
    job_class: JobClass,
    lane: Lane,
    heavy_permit: Option<HeavyPermit>,
    panicked: bool,
}

enum SchedulerEvent {
    Wake,
    Completed(CompletionEvent),
    Shutdown,
}

fn scheduler_loop(
    state: Arc<Mutex<SchedulerState>>,
    heavy: Arc<HeavySemaphore>,
    run_tx: Sender<RunJob>,
    event_rx: Receiver<SchedulerEvent>,
    nonrunnable_dispatches: Arc<AtomicUsize>,
    completed_interactive: Arc<AtomicU64>,
    completed_maintenance: Arc<AtomicU64>,
    dispatch_liveness: Arc<DispatchLivenessAtomics>,
) {
    while let Ok(event) = event_rx.recv() {
        let shutdown;
        {
            let mut state = state.lock();
            shutdown = process_scheduler_event_batch(
                event,
                &event_rx,
                &mut state,
                &completed_interactive,
                &completed_maintenance,
            );

            if !shutdown {
                dispatch_runnable(&mut state, &heavy, &run_tx, &nonrunnable_dispatches);
            }
            dispatch_liveness.record(&state.dispatch_liveness_snapshot());
        }

        if shutdown {
            break;
        }
    }
}

fn process_scheduler_event_batch(
    first: SchedulerEvent,
    event_rx: &Receiver<SchedulerEvent>,
    state: &mut SchedulerState,
    completed_interactive: &AtomicU64,
    completed_maintenance: &AtomicU64,
) -> bool {
    let mut event = Some(first);
    for _ in 0..SCHEDULER_EVENT_BATCH_CAP {
        let Some(current) = event.take().or_else(|| event_rx.try_recv().ok()) else {
            break;
        };
        note_completion_event(&current, completed_interactive, completed_maintenance);
        if process_scheduler_event(current, state) {
            return true;
        }
    }
    false
}

fn note_completion_event(
    event: &SchedulerEvent,
    completed_interactive: &AtomicU64,
    completed_maintenance: &AtomicU64,
) {
    let SchedulerEvent::Completed(event) = event else {
        return;
    };
    match event.job_class {
        JobClass::Interactive => {
            completed_interactive.fetch_add(1, Ordering::Relaxed);
        }
        JobClass::Maintenance => {
            completed_maintenance.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn process_scheduler_event(event: SchedulerEvent, state: &mut SchedulerState) -> bool {
    match event {
        SchedulerEvent::Wake => false,
        SchedulerEvent::Completed(event) => {
            complete_job(state, event);
            false
        }
        SchedulerEvent::Shutdown => true,
    }
}

fn complete_job(state: &mut SchedulerState, event: CompletionEvent) {
    let CompletionEvent {
        root_id,
        request_id,
        job_class,
        lane,
        heavy_permit,
        panicked,
    } = event;
    state.running_jobs.remove(&(root_id.clone(), request_id));

    match job_class {
        JobClass::Interactive => {
            state.interactive_inflight = state.interactive_inflight.saturating_sub(1);
        }
        JobClass::Maintenance => {
            state.maintenance_inflight = state.maintenance_inflight.saturating_sub(1);
        }
    }

    if let Some(actor) = state.actors.get_mut(&root_id) {
        actor.actor_total_inflight = actor.actor_total_inflight.saturating_sub(1);
        match lane {
            Lane::PureRead => {
                actor.read_inflight = actor.read_inflight.saturating_sub(1);
            }
            Lane::SerialLspStatus => {
                actor.lsp_inflight = false;
            }
            Lane::HeavyInit => {}
            Lane::Mutating => {
                actor.writer_inflight = false;
                actor.mutating_inflight = None;
            }
            Lane::MaintenanceCommit => {
                actor.maintenance_commit_inflight = false;
            }
        }

        if panicked && lane == Lane::Mutating {
            actor.fatal = true;
            actor.fail_queued_jobs();
        }
    }

    drop(heavy_permit);
    state.idle_workers += 1;
}

fn dispatch_runnable(
    state: &mut SchedulerState,
    heavy: &Arc<HeavySemaphore>,
    run_tx: &Sender<RunJob>,
    nonrunnable_dispatches: &AtomicUsize,
) {
    while state.idle_workers > 0 && !state.actor_order.is_empty() {
        let mut made_progress = false;
        let mut dispatch_failed = false;

        made_progress |= dispatch_runnable_class(
            state,
            JobClass::Interactive,
            heavy,
            run_tx,
            nonrunnable_dispatches,
            &mut dispatch_failed,
        );
        if dispatch_failed || state.idle_workers == 0 {
            return;
        }

        if can_dispatch_class(state, JobClass::Maintenance) {
            made_progress |= dispatch_runnable_class(
                state,
                JobClass::Maintenance,
                heavy,
                run_tx,
                nonrunnable_dispatches,
                &mut dispatch_failed,
            );
            if dispatch_failed {
                return;
            }
        }

        if !made_progress {
            break;
        }
    }
}

fn dispatch_runnable_class(
    state: &mut SchedulerState,
    job_class: JobClass,
    heavy: &Arc<HeavySemaphore>,
    run_tx: &Sender<RunJob>,
    nonrunnable_dispatches: &AtomicUsize,
    dispatch_failed: &mut bool,
) -> bool {
    if !can_dispatch_class(state, job_class) || state.actor_order.is_empty() {
        return false;
    }

    let actor_count = state.actor_order.len();
    let mut made_progress = false;

    for _ in 0..actor_count {
        if !can_dispatch_class(state, job_class) || state.actor_order.is_empty() {
            break;
        }

        if state.cursor >= state.actor_order.len() {
            state.cursor = 0;
        }
        let root_id = state.actor_order[state.cursor].clone();
        state.cursor = (state.cursor + 1) % state.actor_order.len();

        let run_job = {
            let Some(actor) = state.actors.get_mut(&root_id) else {
                continue;
            };

            if actor.fatal {
                actor.fail_queued_jobs();
                actor.deficit = 0;
                continue;
            }

            if !actor.has_queued_jobs() {
                actor.deficit = 0;
                continue;
            }

            if !actor.has_queued_jobs_for(job_class) {
                continue;
            }

            actor.deficit =
                (actor.deficit + state.config.drr_quantum).min(state.config.deficit_cap);
            if actor.deficit < JOB_COST {
                continue;
            }

            try_admit_actor(&root_id, actor, job_class, &state.config, heavy)
        };

        if let Some(run_job) = run_job {
            state.running_jobs.insert(
                (run_job.root_id.clone(), run_job.request_id.clone()),
                RunningJob {
                    root_id: run_job.root_id.clone(),
                    request_id: run_job.request_id.clone(),
                    command: run_job.command.clone(),
                    job_class: run_job.job_class,
                    lane: run_job.lane,
                    started_at: Instant::now(),
                    execution_started: Arc::clone(&run_job.execution_started),
                    occupancy_reported: false,
                },
            );
            state.idle_workers -= 1;
            match job_class {
                JobClass::Interactive => state.interactive_inflight += 1,
                JobClass::Maintenance => state.maintenance_inflight += 1,
            }
            made_progress = true;
            if run_tx.send(run_job).is_err() {
                nonrunnable_dispatches.fetch_add(1, Ordering::AcqRel);
                *dispatch_failed = true;
                return made_progress;
            }
        }
    }

    made_progress
}

fn can_dispatch_class(state: &SchedulerState, job_class: JobClass) -> bool {
    if state.idle_workers == 0 {
        return false;
    }
    match job_class {
        JobClass::Interactive => true,
        JobClass::Maintenance => {
            state.maintenance_inflight < state.config.maintenance_cap
                && state.idle_workers > state.config.interactive_reserve
        }
    }
}

fn try_admit_actor(
    root_id: &ProjectRootId,
    actor: &mut ActorState,
    job_class: JobClass,
    config: &EffectiveConfig,
    heavy: &Arc<HeavySemaphore>,
) -> Option<RunJob> {
    let lane = match job_class {
        JobClass::Interactive => actor
            .class_queues(JobClass::Interactive)
            .next_interactive_lane(Instant::now())?,
        JobClass::Maintenance => actor.front_lane(job_class)?,
    };
    let mut heavy_permit = None;

    if actor.writer_inflight || actor.higher_priority_writer_barrier_blocks(job_class) {
        return None;
    }

    let has_epoch_reader =
        actor.read_inflight > 0 || actor.lsp_inflight || actor.maintenance_commit_inflight;
    // MaintenanceCommit uses the executor's global maintenance capacity and
    // must not consume the per-actor slots reserved for interactive work. This
    // matters when a two-worker executor has actor_cap=1: the global reserve
    // leaves one worker idle, and this accounting makes that worker reachable
    // by an interactive request for the same actor.
    let actor_interactive_inflight = actor
        .actor_total_inflight
        .saturating_sub(usize::from(actor.maintenance_commit_inflight));
    let actor_has_interactive_capacity = actor_interactive_inflight < config.actor_cap;
    let runnable = match lane {
        Lane::PureRead => actor.read_inflight < config.read_cap && actor_has_interactive_capacity,
        Lane::SerialLspStatus => !actor.lsp_inflight && actor_has_interactive_capacity,
        Lane::HeavyInit => {
            if !actor_has_interactive_capacity {
                false
            } else if let Some(permit) = heavy.try_acquire() {
                heavy_permit = Some(permit);
                true
            } else {
                false
            }
        }
        Lane::Mutating => !has_epoch_reader && actor_has_interactive_capacity,
        // This lane has separate global and per-actor bounds: maintenance_cap
        // reserves workers globally, and the boolean prevents same-actor
        // maintenance from stacking without consuming an interactive slot.
        Lane::MaintenanceCommit => !actor.maintenance_commit_inflight,
    };

    if !runnable {
        return None;
    }

    let promoted_writer_waiting = actor.oldest_queued_writer_at().is_some_and(|queued_at| {
        Instant::now().saturating_duration_since(queued_at) >= INTERACTIVE_WRITER_PROMOTION_AGE
    });
    if promoted_writer_waiting && matches!(lane, Lane::PureRead | Lane::SerialLspStatus) {
        actor.reader_admissions_while_promoted_writer_waited = actor
            .reader_admissions_while_promoted_writer_waited
            .saturating_add(1);
    }

    let queued = actor.pop_front_job(job_class, lane)?;
    actor.deficit -= JOB_COST;
    if lane == Lane::Mutating {
        actor.mutating_inflight = Some(RunningMutatingJob {
            request_id: queued.request_id.clone(),
            command: queued.command.clone(),
            started_at: Instant::now(),
        });
    }
    match lane {
        Lane::PureRead => {
            actor.read_inflight += 1;
            actor.actor_total_inflight += 1;
        }
        Lane::SerialLspStatus => {
            actor.lsp_inflight = true;
            actor.actor_total_inflight += 1;
        }
        Lane::HeavyInit => {
            actor.actor_total_inflight += 1;
        }
        Lane::Mutating => {
            actor.writer_inflight = true;
            actor.actor_total_inflight += 1;
        }
        Lane::MaintenanceCommit => {
            actor.maintenance_commit_inflight = true;
            actor.actor_total_inflight += 1;
        }
    }

    Some(RunJob {
        root_id: root_id.clone(),
        job_class,
        lane,
        ctx: Arc::clone(&actor.ctx),
        epoch: Arc::clone(&actor.epoch),
        job: queued.job,
        completion: Some(queued.completion),
        request_id: queued.request_id,
        command: queued.command,
        heavy_permit,
        cancellation: queued.cancellation,
        execution_started: Arc::new(AtomicBool::new(false)),
    })
}

fn worker_loop(run_rx: Receiver<RunJob>, event_tx: Sender<SchedulerEvent>) {
    while let Ok(mut run_job) = run_rx.recv() {
        let response =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_lane_job(&mut run_job)));
        let panicked = response.is_err();
        let response = match response {
            Ok(response) => response,
            Err(payload) => panic_response(
                run_job.request_id.clone(),
                &run_job.command,
                payload.as_ref(),
            ),
        };

        if let Some(completion) = run_job.completion.take() {
            completion.send(response);
        }
        let completion = CompletionEvent {
            root_id: run_job.root_id,
            request_id: run_job.request_id,
            job_class: run_job.job_class,
            lane: run_job.lane,
            heavy_permit: run_job.heavy_permit.take(),
            panicked,
        };
        let _ = event_tx.send(SchedulerEvent::Completed(completion));
    }
}

fn run_lane_job(run_job: &mut RunJob) -> Response {
    let _cancellation_ctx = JobCancellationContextGuard::install(run_job.cancellation.clone());
    let missing_request_id = run_job.request_id.clone();
    let job = std::mem::replace(
        &mut run_job.job,
        Box::new(move |_| {
            Response::error(
                missing_request_id,
                "job_missing",
                "executor job already taken",
            )
        }),
    );

    let run = |job: ExecutorJob| {
        let can_start = run_job
            .cancellation
            .as_ref()
            .is_none_or(JobCancellation::mark_running);
        if !can_start {
            return Response::error(
                run_job.request_id.clone(),
                "request_cancelled",
                "request cancelled before execution",
            );
        }
        run_job.execution_started.store(true, Ordering::Release);
        job(&run_job.ctx)
    };

    match run_job.lane {
        Lane::PureRead | Lane::SerialLspStatus => {
            if let Some(cancellation) = run_job.cancellation.as_ref() {
                loop {
                    if let Some(_epoch) = run_job.epoch.try_read_for(Duration::from_millis(25)) {
                        return run(job);
                    }
                    if cancellation.cancel_requested_before_commit() {
                        return Response::error(
                            run_job.request_id.clone(),
                            "request_cancelled",
                            "request cancelled before execution",
                        );
                    }
                }
            }
            let _epoch = run_job.epoch.read();
            run(job)
        }
        // Cache and index builders reject stale generations and serialize
        // publication internally. Taking the actor write gate after the command
        // constructs its response would delay completion behind maintenance and
        // park later readers behind an otherwise unnecessary writer.
        Lane::HeavyInit => run(job),
        Lane::Mutating => {
            let _epoch = run_job.epoch.write();
            run(job)
        }
        Lane::MaintenanceCommit => {
            // Same gate as reads: the job's mutations are protected by the
            // touched subsystems' own locks, and holding only the read gate
            // lets interactive PureReads overlap freely.
            let _epoch = run_job.epoch.read();
            run(job)
        }
    }
}

#[derive(Debug)]
struct HeavySemaphore {
    available: AtomicUsize,
    max: usize,
}

impl HeavySemaphore {
    fn new(permits: usize) -> Self {
        Self {
            available: AtomicUsize::new(permits),
            max: permits,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<HeavyPermit> {
        loop {
            let available = self.available.load(Ordering::Acquire);
            if available == 0 {
                return None;
            }
            if self
                .available
                .compare_exchange(
                    available,
                    available - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(HeavyPermit {
                    semaphore: Arc::clone(self),
                });
            }
        }
    }
}

struct HeavyPermit {
    semaphore: Arc<HeavySemaphore>,
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        let previous = self.semaphore.available.fetch_add(1, Ordering::Release);
        debug_assert!(previous < self.semaphore.max);
    }
}
