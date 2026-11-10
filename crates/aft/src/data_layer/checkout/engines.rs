//! The call-graph engines a data layer keeps between refreshes.
//!
//! A checkout's engine holds what lets a refresh redo only what an edit
//! reaches: every file it has bound and every file's method-call inputs it
//! has computed, which a new engine works out again on its first refresh,
//! seconds of CPU on a large repository. So an engine outlives each
//! publication of its checkout's changes, moved onto the new publication
//! (see `Checkout::carry_engine`), and goes only when:
//!
//! - its checkout has recorded no edit and no refresh has applied anything
//!   to it for [`ENGINE_IDLE`];
//! - the engines of every checkout of the data layer together hold more
//!   than [`ENGINE_BUDGET`], counted as the memory census counts them: then
//!   the engines whose checkouts were used least recently go first, never
//!   one a refresh is using;
//! - its checkout is unbound, which drops its slot.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use super::GraphEngine;

/// How long an engine stays after its checkout's last edit or applied
/// refresh. A coding agent's edits come seconds to minutes apart, and a
/// checkout publishes its changes two minutes after its last edit; an
/// engine kept past that serves the next burst of edits without starting
/// again.
pub(super) const ENGINE_IDLE: Duration = Duration::from_secs(10 * 60);

/// How many bytes the engines of one data layer may hold together before
/// the least recently used idle ones go. Measured as the budget counts
/// them: on AFT's own checkout (7,456 files) an engine holds about 105 MB
/// after its first refresh, nearly all of it the files it bound; kept
/// across every publication of its changes (62 of them) in a 430-edit
/// replay of AFT's history it held 158 MiB at the median, 307 MiB at the
/// 90th percentile and 344 MiB at most. A gibibyte keeps the engines of
/// three worktrees refreshing at once, and of a few more agents'
/// worktrees, without letting any go.
pub(super) const ENGINE_BUDGET: u64 = 1 << 30;

/// How old an engine's measured bytes may be before a refresh measures it
/// again. Walking a large engine takes tens of milliseconds, a few percent
/// of a refresh, and an engine grows slowly between publications, so most
/// refreshes reuse the cached byte estimate. Publication can measure sooner
/// after releasing substantial state, or when the last estimate still
/// includes state held only before publication.
const MEASURE_EVERY_MS: u64 = 30_000;

/// [`EngineSlot::measured_ms`] for an engine not measured yet.
const NEVER: u64 = u64::MAX;

/// Why an engine went, for the count the release log line carries.
#[derive(Clone, Copy)]
enum Release {
    Idle,
    Budget,
    Unbind,
}

/// How many engines this process let go for each [`Release`], so the log
/// shows how often the budget, rather than idleness or an unbind, sends
/// one away.
static RELEASED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Counts a release for `reason` and logs it with every count so far.
fn log_release(root: &std::path::Path, bytes: u64, reason: Release, why: &str) {
    RELEASED[reason as usize].fetch_add(1, Ordering::AcqRel);
    let [idle, budget, unbind] = &RELEASED;
    crate::slog_info!(
        "data layer released the call-graph engine of {} ({} MiB): {why}; engines released so far: {} idle, {} over budget, {} unbound",
        root.display(),
        bytes >> 20,
        idle.load(Ordering::Acquire),
        budget.load(Ordering::Acquire),
        unbind.load(Ordering::Acquire)
    );
}

/// One checkout's engine, shared by the checkout and its data layer's
/// [`EngineKeeper`]. Dereferences to the engine's lock.
pub(super) struct EngineSlot {
    engine: Mutex<Option<GraphEngine>>,
    /// The checkout's root, for the log line when an engine goes.
    root: PathBuf,
    /// When the checkout last recorded an edit, or a refresh applied
    /// something to its engine, in milliseconds since the keeper started.
    used_ms: AtomicU64,
    /// The engine's bytes when it was last measured (see
    /// [`EngineKeeper::kept`]).
    bytes: AtomicU64,
    /// When they were measured, in milliseconds since the keeper started;
    /// [`NEVER`] until the slot's engine is first measured.
    measured_ms: AtomicU64,
    /// The checkout's memory epoch, moved when the keeper lets the engine
    /// go so the census walks the checkout again.
    memory_epoch: Arc<AtomicU64>,
    /// How many engines refreshes have started in this slot.
    starts: AtomicU64,
}

impl std::ops::Deref for EngineSlot {
    type Target = Mutex<Option<GraphEngine>>;

    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

impl Drop for EngineSlot {
    /// The checkout is unbound: its engine goes with it.
    fn drop(&mut self) {
        let engine = self
            .engine
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if engine.take().is_some() {
            let bytes = *self.bytes.get_mut();
            log_release(
                &self.root,
                bytes,
                Release::Unbind,
                "its checkout was unbound",
            );
        }
    }
}

impl EngineSlot {
    /// The cached census figure the budget currently charges to this slot.
    pub(super) fn budget_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }

    /// Notes that a refresh started a new engine in this slot, which is
    /// measured when it is first kept.
    pub(super) fn note_start(&self) {
        self.starts.fetch_add(1, Ordering::AcqRel);
        self.measured_ms.store(NEVER, Ordering::Release);
    }

    /// How many engines refreshes have started in this slot.
    pub(super) fn starts(&self) -> u64 {
        self.starts.load(Ordering::Acquire)
    }

    /// Lets the engine go for `reason`, unless a refresh holds it now; true
    /// when one went.
    fn release(&self, reason: Release, why: &str) -> bool {
        let mut engine = match self.engine.try_lock() {
            Ok(engine) => engine,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        let Some(released) = engine.take() else {
            return false;
        };
        drop(engine);
        let bytes = self.bytes.swap(0, Ordering::AcqRel);
        drop(released);
        self.memory_epoch.fetch_add(1, Ordering::AcqRel);
        log_release(&self.root, bytes, reason, why);
        true
    }

    /// Whether the slot holds an engine, and whether a refresh holds the
    /// slot now (it is counted as holding one then).
    fn held(&self) -> (bool, bool) {
        match self.engine.try_lock() {
            Ok(engine) => (engine.is_some(), false),
            Err(TryLockError::Poisoned(poisoned)) => (poisoned.into_inner().is_some(), false),
            Err(TryLockError::WouldBlock) => (true, true),
        }
    }
}

/// Every engine slot of one data layer, the thread that lets idle engines
/// go, and the budget they share.
pub(super) struct EngineKeeper {
    started: Instant,
    idle_ms: AtomicU64,
    budget: AtomicU64,
    state: Mutex<KeeperState>,
    wake: Condvar,
}

#[derive(Default)]
struct KeeperState {
    slots: Vec<Weak<EngineSlot>>,
    /// The thread that lets idle engines go is running.
    reaping: bool,
    /// An engine was kept since that thread last looked.
    kept: bool,
    /// The data layer is gone; the thread ends.
    stopped: bool,
}

impl std::fmt::Debug for EngineKeeper {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineKeeper")
            .field("idle_ms", &self.idle_ms.load(Ordering::Relaxed))
            .field("budget", &self.budget.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl EngineKeeper {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            idle_ms: AtomicU64::new(u64::try_from(ENGINE_IDLE.as_millis()).unwrap_or(u64::MAX)),
            budget: AtomicU64::new(ENGINE_BUDGET),
            state: Mutex::new(KeeperState::default()),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, KeeperState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// A new empty slot for the checkout at `root`, whose memory epoch is
    /// `memory_epoch`.
    pub(super) fn slot(&self, root: PathBuf, memory_epoch: Arc<AtomicU64>) -> Arc<EngineSlot> {
        let slot = Arc::new(EngineSlot {
            engine: Mutex::new(None),
            root,
            used_ms: AtomicU64::new(self.now_ms()),
            bytes: AtomicU64::new(0),
            measured_ms: AtomicU64::new(NEVER),
            memory_epoch,
            starts: AtomicU64::new(0),
        });
        let mut state = self.lock();
        state.slots.retain(|slot| slot.strong_count() > 0);
        state.slots.push(Arc::downgrade(&slot));
        slot
    }

    /// Notes that `slot`'s checkout was used now: an edit recorded, or a
    /// refresh applied something to its engine.
    pub(super) fn touch(&self, slot: &EngineSlot) {
        slot.used_ms.store(self.now_ms(), Ordering::Release);
    }

    /// Whether the engine in `slot` is to be measured as it is kept: it has
    /// not been yet, or its figure is older than [`MEASURE_EVERY_MS`].
    pub(super) fn measure_due(&self, slot: &EngineSlot) -> bool {
        let measured = slot.measured_ms.load(Ordering::Acquire);
        measured == NEVER || self.now_ms().saturating_sub(measured) >= MEASURE_EVERY_MS
    }

    /// Notes that `slot` keeps an engine, now that a refresh applied
    /// something to it or it moved onto a new publication, with its bytes
    /// as the census counts them when the refresh or publication measured it
    /// (the last figure stands otherwise). Lets the least recently used
    /// other engines go while all of them together hold more than the
    /// budget, and makes sure the thread that lets idle engines go is
    /// running.
    pub(super) fn kept(self: &Arc<Self>, slot: &Arc<EngineSlot>, bytes: Option<u64>) {
        if let Some(bytes) = bytes {
            slot.bytes.store(bytes, Ordering::Release);
            slot.measured_ms.store(self.now_ms(), Ordering::Release);
        }
        self.enforce_budget(slot);
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        if state.reaping {
            state.kept = true;
            self.wake.notify_all();
            return;
        }
        let keeper = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("aft-data-engine-idle".to_string())
            .spawn(move || keeper.reap());
        match spawned {
            Ok(_) => state.reaping = true,
            Err(error) => crate::slog_warn!(
                "data layer could not start the thread that lets idle call-graph engines go: {error}"
            ),
        }
    }

    fn live_slots(&self) -> Vec<Arc<EngineSlot>> {
        let mut state = self.lock();
        state.slots.retain(|slot| slot.strong_count() > 0);
        state.slots.iter().filter_map(Weak::upgrade).collect()
    }

    /// Lets the least recently used engines other than `current`'s go
    /// until every engine together holds no more than the budget. An engine
    /// a refresh holds counts, but cannot go.
    fn enforce_budget(&self, current: &Arc<EngineSlot>) {
        let budget = self.budget.load(Ordering::Acquire);
        let slots = self.live_slots();
        let mut total = 0u64;
        let mut idle = Vec::new();
        for slot in &slots {
            let (held, busy) = slot.held();
            if !held {
                continue;
            }
            total = total.saturating_add(slot.bytes.load(Ordering::Acquire));
            if !busy && !Arc::ptr_eq(slot, current) {
                idle.push((slot.used_ms.load(Ordering::Acquire), slot));
            }
        }
        if total <= budget {
            return;
        }
        idle.sort_by_key(|(used, _)| *used);
        for (_, slot) in idle {
            if total <= budget {
                break;
            }
            let bytes = slot.bytes.load(Ordering::Acquire);
            let why = format!(
                "the engines held {} MiB, over the budget of {} MiB, and this checkout's was the least recently used",
                total >> 20,
                budget >> 20
            );
            if slot.release(Release::Budget, &why) {
                total = total.saturating_sub(bytes);
            }
        }
    }

    /// The thread that lets idle engines go: sleeps until the earliest
    /// time an engine falls idle, lets every idle one go, and ends once no
    /// slot holds an engine.
    fn reap(self: Arc<Self>) {
        loop {
            let idle = self.idle_ms.load(Ordering::Acquire);
            let now = self.now_ms();
            let mut next: Option<u64> = None;
            for slot in self.live_slots() {
                let (held, busy) = slot.held();
                if !held {
                    continue;
                }
                let due = if busy {
                    now.saturating_add(idle)
                } else {
                    slot.used_ms.load(Ordering::Acquire).saturating_add(idle)
                };
                if due <= now {
                    let why = format!("no edit or refresh for {} s", idle / 1000);
                    if slot.release(Release::Idle, &why) {
                        continue;
                    }
                    // A refresh took it meanwhile.
                    next = Some(next.map_or(now + idle, |next| next.min(now + idle)));
                    continue;
                }
                next = Some(next.map_or(due, |next| next.min(due)));
            }
            let mut state = self.lock();
            if state.stopped {
                state.reaping = false;
                return;
            }
            if std::mem::take(&mut state.kept) {
                continue;
            }
            let Some(next) = next else {
                state.reaping = false;
                return;
            };
            let wait = Duration::from_millis(next.saturating_sub(self.now_ms()).max(1));
            let (mut state, _) = self
                .wake
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.kept = false;
            if state.stopped {
                state.reaping = false;
                return;
            }
        }
    }

    /// Ends the thread that lets idle engines go: the data layer is gone.
    pub(super) fn stop(&self) {
        self.lock().stopped = true;
        self.wake.notify_all();
    }

    /// Sets how long an engine stays after its checkout's last use, for
    /// tests and measurement.
    pub(super) fn set_idle(&self, idle: Duration) {
        self.idle_ms.store(
            u64::try_from(idle.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
        self.wake.notify_all();
    }

    /// Sets the bytes every engine together may hold, for tests and
    /// measurement.
    pub(super) fn set_budget(&self, bytes: u64) {
        self.budget.store(bytes, Ordering::Release);
    }
}
