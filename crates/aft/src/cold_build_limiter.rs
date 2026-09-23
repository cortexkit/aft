use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(not(test))]
const DEFAULT_COLD_BUILD_LIMIT: usize = 2;
#[cfg(test)]
const DEFAULT_COLD_BUILD_LIMIT: usize = 1024;

// This is an internal harness record, not a user-facing limiter setting. The
// test harness performs 32 release/admission cycles, so retain enough events to
// cover that exercise while bounding memory use in a long-lived daemon.
const ADMISSION_EVENT_RETENTION: usize = 64;

static GLOBAL_COLD_BUILD_LIMITER: LazyLock<Arc<ColdBuildLimiter>> =
    LazyLock::new(|| Arc::new(ColdBuildLimiter::new(DEFAULT_COLD_BUILD_LIMIT)));

pub(crate) fn global_limiter() -> Arc<ColdBuildLimiter> {
    Arc::clone(&GLOBAL_COLD_BUILD_LIMITER)
}

pub(crate) fn isolated_limiter(limit: usize) -> Arc<ColdBuildLimiter> {
    Arc::new(ColdBuildLimiter::new(limit))
}

pub fn try_acquire() -> Option<ColdBuildPermit> {
    GLOBAL_COLD_BUILD_LIMITER.try_acquire()
}

/// Block until a build slot is free, then take it.
///
/// For build sites with no reschedule path (search-index builds spawn once per
/// configure): skipping would strand the index, so past-cap work waits instead.
/// Production captures showed concurrent per-root builds starving dispatch
/// while CPU sat idle; waiting serializes that pressure at the source. Only
/// call from dedicated background threads, never the dispatch thread or an
/// executor worker.
pub fn acquire_blocking(kind: &str) -> ColdBuildPermit {
    acquire_blocking_while(kind, || true).expect("unconditional cold-build admission")
}

/// Wait for a build slot while `admitted` remains true. The predicate is checked
/// before every attempt, so a root that becomes unbound does not consume a slot
/// after spending time queued behind the process-wide cap.
pub fn acquire_blocking_while(kind: &str, admitted: impl Fn() -> bool) -> Option<ColdBuildPermit> {
    acquire_blocking_while_with_limiter(&GLOBAL_COLD_BUILD_LIMITER, kind, admitted)
}

pub(crate) fn acquire_blocking_while_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    admitted: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    let request = ColdBuildAdmissionRequest::new(kind, ColdBuildAdmissionClass::Maintenance);
    acquire_blocking_while_inner(limiter, kind, Some(&request), admitted, || false)
}

/// Same admission as [`acquire_blocking_while_with_limiter`], but the holder
/// and queue census name `root` instead of `unknown`. The root is a label only:
/// unlike [`ColdBuildAdmissionRequest::for_root`] it does not let other builds
/// for the same root share this permit, so slot accounting is unchanged.
pub(crate) fn acquire_blocking_while_for_root_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    root: &std::path::Path,
    admitted: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    let request = ColdBuildAdmissionRequest::labelled_with_root(
        root.display().to_string(),
        kind,
        ColdBuildAdmissionClass::Maintenance,
    );
    acquire_blocking_while_inner(limiter, kind, Some(&request), admitted, || false)
}

/// Identify the source of a cold-build request without exposing a limiter knob.
///
/// The classes deliberately have no absolute priority ordering. When a class
/// was admitted most recently and another class is waiting, the limiter defers
/// that repeat admission. Standing adds a yielding class to this existing
/// rotation; it never installs a priority retry path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColdBuildAdmissionClass {
    InspectTriggered,
    Maintenance,
    Standing,
}

const ADMISSION_CLASS_COUNT: usize = 3;

impl ColdBuildAdmissionClass {
    const fn index(self) -> usize {
        match self {
            Self::InspectTriggered => 0,
            Self::Maintenance => 1,
            Self::Standing => 2,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::InspectTriggered => "inspect-triggered",
            Self::Maintenance => "maintenance",
            Self::Standing => "standing",
        }
    }
}

/// Internal request metadata attached to an admission attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ColdBuildAdmissionRequest {
    request_id: String,
    /// Root whose builds share one permit (see `holders_by_root`).
    root: Option<String>,
    /// Root shown in the census when `root` is unset. Display only.
    census_root: Option<String>,
    class: ColdBuildAdmissionClass,
}

impl ColdBuildAdmissionRequest {
    pub(crate) fn new(request_id: impl Into<String>, class: ColdBuildAdmissionClass) -> Self {
        let request_id = request_id.into();
        let root = request_id
            .strip_prefix("inspect:")
            .and_then(|value| value.rsplit_once(':').map(|(root, _)| root.to_string()));
        Self {
            request_id,
            root,
            census_root: None,
            class,
        }
    }

    pub(crate) fn for_root(
        root: impl Into<String>,
        request_id: impl Into<String>,
        class: ColdBuildAdmissionClass,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            root: Some(root.into()),
            census_root: None,
            class,
        }
    }

    /// A request whose census entry names `root` without joining that root's
    /// shared permit.
    pub(crate) fn labelled_with_root(
        root: impl Into<String>,
        request_id: impl Into<String>,
        class: ColdBuildAdmissionClass,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            root: None,
            census_root: Some(root.into()),
            class,
        }
    }

    fn census_label(&self) -> Option<&str> {
        self.root.as_deref().or(self.census_root.as_deref())
    }
}

/// A structured record of a successful cold-build admission.
///
/// `admission_order` records the order in which permits were admitted, not the
/// order in which waiters arrived. Arrival-order and overtake checks require a
/// separate ticketed-ordering design.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ColdBuildAdmissionEvent {
    pub(crate) request_id: String,
    pub(crate) class: ColdBuildAdmissionClass,
    pub(crate) admission_order: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ColdBuildCensusEntry {
    pub(crate) domain: &'static str,
    pub(crate) root: String,
    pub(crate) kind: String,
    pub(crate) sharers: usize,
    pub(crate) acquired_at_ms: u64,
    pub(crate) age_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ColdBuildLimiterCensus {
    pub(crate) cap: usize,
    pub(crate) holders: Vec<ColdBuildCensusEntry>,
    pub(crate) queued: Vec<ColdBuildCensusEntry>,
}

/// Attempt immediate admission without bypassing queued requests from the other
/// class. Background schedulers use this when they can defer rejected work.
pub(crate) fn try_acquire_classified_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    request: &ColdBuildAdmissionRequest,
) -> Option<ColdBuildPermit> {
    limiter.try_acquire_classified(request, &request.request_id, || true)
}

/// Acquire a limiter permit for a classified request while it remains admitted
/// and uncancelled.
///
/// Cancellation is sampled before every acquisition attempt and again after a
/// permit has been acquired. The second check closes the gap before a build can
/// start: a newly cancelled request returns the permit without emitting an
/// admission event.
pub(crate) fn acquire_blocking_while_cancellable_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    request: ColdBuildAdmissionRequest,
    admitted: impl Fn() -> bool,
    cancelled: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    acquire_blocking_while_inner(limiter, kind, Some(&request), admitted, cancelled)
}

/// A Standing permit preserves the lifecycle admission epoch captured before
/// limiter acquisition. Checkpoint code drops it before yielding and carries the
/// same epoch into the next attempt, so an obsolete build cannot become current
/// merely by waiting for a slot.
#[derive(Debug)]
pub(crate) struct StandingColdBuildPermit {
    _permit: ColdBuildPermit,
    pub(crate) admission_epoch: u64,
}

/// Standing performs the same waiter inspection before initial acquisition and
/// checkpoint reacquisition because both call this one function. It declines
/// immediately when an interactive or normal-maintenance waiter is visible.
pub(crate) fn acquire_standing_while_cancellable_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    request_id: impl Into<String>,
    admission_epoch: u64,
    admitted: impl Fn() -> bool,
    cancelled: impl Fn() -> bool,
) -> Option<StandingColdBuildPermit> {
    let request = ColdBuildAdmissionRequest::new(request_id, ColdBuildAdmissionClass::Standing);
    acquire_blocking_while_inner(limiter, kind, Some(&request), admitted, cancelled).map(|permit| {
        StandingColdBuildPermit {
            _permit: permit,
            admission_epoch,
        }
    })
}

fn acquire_blocking_while_inner(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    request: Option<&ColdBuildAdmissionRequest>,
    admitted: impl Fn() -> bool,
    cancelled: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    let _waiter = request.map(|request| AdmissionWaiter::register(limiter, request, kind));
    let started = Instant::now();
    let mut logged = false;
    loop {
        if !admitted() || cancelled() {
            return None;
        }
        // Standing yields to any already-queued interactive or ordinary
        // maintenance contender. Returning None keeps it resumable; callers
        // use the same path again at the next checkpoint without priority tags.
        if request.is_some_and(|request| request.class == ColdBuildAdmissionClass::Standing)
            && limiter.has_non_standing_waiters()
        {
            return None;
        }
        let revoked_after_acquire = std::cell::Cell::new(false);
        let permit = match request {
            Some(request) => limiter.try_acquire_classified(request, kind, || {
                let still_admitted = admitted()
                    && !cancelled()
                    && (request.class != ColdBuildAdmissionClass::Standing
                        || !limiter.has_non_standing_waiters());
                revoked_after_acquire.set(!still_admitted);
                still_admitted
            }),
            None => limiter.try_acquire().and_then(|permit| {
                // A request can become unbound or cancelled after the pre-attempt
                // check but before the permit is acquired. Recheck while owning
                // the slot; dropping the permit returns it before any build starts.
                if admitted() && !cancelled() {
                    Some(permit)
                } else {
                    revoked_after_acquire.set(true);
                    drop(permit);
                    None
                }
            }),
        };
        if revoked_after_acquire.get() {
            return None;
        }
        if let Some(permit) = permit {
            let wait_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            if wait_ms > 0 {
                crate::logging::note_tool_call_wait(
                    crate::run_tool_call::WaitingOn::Limiter,
                    None,
                    wait_ms,
                );
            }
            if logged {
                match request {
                    Some(request) => crate::slog_info!(
                        "{} cold-build slot acquired after {}ms wait: request={} kind={}",
                        request.class.label(),
                        wait_ms,
                        request.request_id,
                        kind
                    ),
                    None => crate::slog_info!(
                        "maintenance build slot acquired after {}ms wait: {}",
                        wait_ms,
                        kind
                    ),
                }
            }
            return Some(permit);
        }
        if !logged {
            match request {
                Some(request) => crate::slog_info!(
                    "{} cold-build request queued behind concurrency cap ({}): request={} kind={}",
                    request.class.label(),
                    limiter.limit(),
                    request.request_id,
                    kind
                ),
                None => crate::slog_info!(
                    "maintenance build queued behind concurrency cap ({}): {}",
                    limiter.limit(),
                    kind
                ),
            }
            logged = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn limit() -> usize {
    GLOBAL_COLD_BUILD_LIMITER.limit()
}

#[cfg(test)]
pub(crate) fn test_limiter(limit: usize) -> Arc<ColdBuildLimiter> {
    Arc::new(ColdBuildLimiter::new(limit))
}

#[cfg(test)]
pub(crate) fn acquire_blocking_while_with_test_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    admitted: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    acquire_blocking_while_with_limiter(limiter, kind, admitted)
}

#[derive(Debug)]
pub(crate) struct ColdBuildLimiter {
    available: AtomicUsize,
    limit: usize,
    /// Waiter counts are atomics so a Standing contender can yield without a
    /// second hot-path lock. Rotation still uses `admission_state` below.
    waiting_by_class: [AtomicUsize; ADMISSION_CLASS_COUNT],
    admission_state: Mutex<AdmissionState>,
}

#[derive(Debug)]
struct AdmissionState {
    last_admitted_class: Option<ColdBuildAdmissionClass>,
    next_admission_order: u64,
    next_census_id: u64,
    events: VecDeque<ColdBuildAdmissionEvent>,
    holders: BTreeMap<u64, CensusRecord>,
    holders_by_root: BTreeMap<String, RootHolder>,
    queued: BTreeMap<u64, CensusRecord>,
}

#[derive(Debug)]
struct RootHolder {
    census_id: u64,
    permit: Weak<ColdBuildPermitLease>,
}

#[derive(Clone, Debug)]
struct CensusRecord {
    domain: &'static str,
    root: String,
    kind: String,
    permit: Option<Weak<ColdBuildPermitLease>>,
    started_at_ms: u64,
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl ColdBuildLimiter {
    fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            available: AtomicUsize::new(limit),
            limit,
            waiting_by_class: std::array::from_fn(|_| AtomicUsize::new(0)),
            admission_state: Mutex::new(AdmissionState {
                last_admitted_class: None,
                next_admission_order: 1,
                next_census_id: 1,
                events: VecDeque::with_capacity(ADMISSION_EVENT_RETENTION),
                holders: BTreeMap::new(),
                holders_by_root: BTreeMap::new(),
                queued: BTreeMap::new(),
            }),
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    fn try_take_slot(&self) -> bool {
        loop {
            let available = self.available.load(Ordering::Acquire);
            if available == 0 {
                return false;
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
                return true;
            }
        }
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ColdBuildPermit> {
        if !self.try_take_slot() {
            return None;
        }
        let mut state = self
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(self.install_permit_locked(&mut state, "unclassified", None, None, "unclassified"))
    }

    fn try_acquire_classified(
        self: &Arc<Self>,
        request: &ColdBuildAdmissionRequest,
        kind: &str,
        admitted_after_acquire: impl FnOnce() -> bool,
    ) -> Option<ColdBuildPermit> {
        let mut state = self
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(root) = request.root.as_deref() {
            if let Some(holder) = state.holders_by_root.get(root) {
                let Some(lease) = holder.permit.upgrade() else {
                    // The final sharer is dropping and will return the slot after
                    // removing this entry. Retry rather than briefly double-owning
                    // capacity for the same root on another free slot.
                    return None;
                };
                if !admitted_after_acquire() {
                    drop(state);
                    drop(lease);
                    return None;
                }
                return Some(ColdBuildPermit { lease });
            }
        }

        // Class alternation arbitrates the final released slot. When several
        // slots are free, admitting both classes is not starvation and avoids
        // stranding independent roots behind an artificial one-at-a-time turn.
        let available = self.available.load(Ordering::Acquire);
        if available <= 1
            && self.has_waiter_from_another_class(request.class)
            && state.last_admitted_class == Some(request.class)
        {
            return None;
        }
        if !self.try_take_slot() {
            return None;
        }
        if !admitted_after_acquire() {
            self.available.fetch_add(1, Ordering::Release);
            return None;
        }
        Self::record_admission_locked(&mut state, request);
        Some(self.install_permit_locked(
            &mut state,
            request.class.label(),
            request.root.as_deref(),
            request.census_label(),
            kind,
        ))
    }

    fn install_permit_locked(
        self: &Arc<Self>,
        state: &mut AdmissionState,
        domain: &'static str,
        root: Option<&str>,
        census_root: Option<&str>,
        kind: &str,
    ) -> ColdBuildPermit {
        let census_id = state.next_census_id;
        state.next_census_id = state.next_census_id.saturating_add(1);
        let lease = Arc::new(ColdBuildPermitLease {
            limiter: Arc::clone(self),
            census_id,
            root: root.map(ToOwned::to_owned),
        });
        let weak = Arc::downgrade(&lease);
        state.holders.insert(
            census_id,
            CensusRecord {
                domain,
                root: census_root.unwrap_or("unknown").to_string(),
                kind: kind.to_string(),
                permit: Some(Weak::clone(&weak)),
                started_at_ms: unix_millis_now(),
            },
        );
        if let Some(root) = root {
            state.holders_by_root.insert(
                root.to_string(),
                RootHolder {
                    census_id,
                    permit: weak,
                },
            );
        }
        ColdBuildPermit { lease }
    }

    fn record_admission_locked(state: &mut AdmissionState, request: &ColdBuildAdmissionRequest) {
        let event = ColdBuildAdmissionEvent {
            request_id: request.request_id.clone(),
            class: request.class,
            admission_order: state.next_admission_order,
        };
        state.next_admission_order += 1;
        state.last_admitted_class = Some(request.class);
        if state.events.len() == ADMISSION_EVENT_RETENTION {
            state.events.pop_front();
        }
        state.events.push_back(event);
    }

    /// Expose recorded admissions to internal tests and harness code so they
    /// can verify limiter behavior without parsing log output.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn admission_events(&self) -> Vec<ColdBuildAdmissionEvent> {
        self.admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .iter()
            .cloned()
            .collect()
    }

    /// O(1) waiter-set inspection used by Standing before its initial permit
    /// and every checkpoint reacquisition. The counters are maintained by RAII
    /// waiters and therefore need no scheduler-state lock on this hot path.
    pub(crate) fn census(&self) -> ColdBuildLimiterCensus {
        let now_ms = unix_millis_now();
        let state = self
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let render = |entry: &CensusRecord| ColdBuildCensusEntry {
            domain: entry.domain,
            root: entry.root.clone(),
            kind: entry.kind.clone(),
            sharers: entry
                .permit
                .as_ref()
                .map_or(1, |permit| permit.strong_count().max(1)),
            acquired_at_ms: entry.started_at_ms,
            age_ms: now_ms.saturating_sub(entry.started_at_ms),
        };
        ColdBuildLimiterCensus {
            cap: self.limit,
            holders: state.holders.values().map(render).collect(),
            queued: state.queued.values().map(render).collect(),
        }
    }

    pub(crate) fn has_non_standing_waiters(&self) -> bool {
        self.waiting_by_class[ColdBuildAdmissionClass::InspectTriggered.index()]
            .load(Ordering::Acquire)
            > 0
            || self.waiting_by_class[ColdBuildAdmissionClass::Maintenance.index()]
                .load(Ordering::Acquire)
                > 0
    }

    fn has_waiter_from_another_class(&self, class: ColdBuildAdmissionClass) -> bool {
        self.waiting_by_class
            .iter()
            .enumerate()
            .any(|(index, waiters)| index != class.index() && waiters.load(Ordering::Acquire) > 0)
    }

    #[cfg(test)]
    fn waiting_by_class_for_test(&self) -> [usize; ADMISSION_CLASS_COUNT] {
        std::array::from_fn(|index| self.waiting_by_class[index].load(Ordering::Acquire))
    }
}

struct AdmissionWaiter {
    limiter: Arc<ColdBuildLimiter>,
    class: ColdBuildAdmissionClass,
    census_id: u64,
}

impl AdmissionWaiter {
    fn register(
        limiter: &Arc<ColdBuildLimiter>,
        request: &ColdBuildAdmissionRequest,
        kind: &str,
    ) -> Self {
        limiter.waiting_by_class[request.class.index()].fetch_add(1, Ordering::AcqRel);
        let mut state = limiter
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let census_id = state.next_census_id;
        state.next_census_id = state.next_census_id.saturating_add(1);
        state.queued.insert(
            census_id,
            CensusRecord {
                domain: request.class.label(),
                root: request.census_label().unwrap_or("unknown").to_string(),
                kind: kind.to_string(),
                permit: None,
                started_at_ms: unix_millis_now(),
            },
        );
        drop(state);
        Self {
            limiter: Arc::clone(limiter),
            class: request.class,
            census_id,
        }
    }
}

impl Drop for AdmissionWaiter {
    fn drop(&mut self) {
        let previous =
            self.limiter.waiting_by_class[self.class.index()].fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        self.limiter
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued
            .remove(&self.census_id);
    }
}

#[derive(Clone, Debug)]
pub struct ColdBuildPermit {
    lease: Arc<ColdBuildPermitLease>,
}

impl ColdBuildPermit {
    pub(crate) fn downgrade(&self) -> WeakColdBuildPermit {
        WeakColdBuildPermit {
            lease: Arc::downgrade(&self.lease),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WeakColdBuildPermit {
    lease: Weak<ColdBuildPermitLease>,
}

impl WeakColdBuildPermit {
    pub(crate) fn upgrade(&self) -> Option<ColdBuildPermit> {
        self.lease.upgrade().map(|lease| ColdBuildPermit { lease })
    }
}

#[derive(Debug)]
struct ColdBuildPermitLease {
    limiter: Arc<ColdBuildLimiter>,
    census_id: u64,
    root: Option<String>,
}

impl Drop for ColdBuildPermitLease {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.holders.remove(&self.census_id);
        if let Some(root) = self.root.as_deref() {
            let owns_root_entry = state
                .holders_by_root
                .get(root)
                .is_some_and(|holder| holder.census_id == self.census_id);
            if owns_root_entry {
                state.holders_by_root.remove(root);
            }
        }
        drop(state);
        let previous = self.limiter.available.fetch_add(1, Ordering::Release);
        debug_assert!(previous < self.limiter.limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process-global limiter; run them one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_for_waiters(limiter: &ColdBuildLimiter) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let waiting = limiter.waiting_by_class_for_test();
            if waiting[ColdBuildAdmissionClass::InspectTriggered.index()] > 0
                && waiting[ColdBuildAdmissionClass::Maintenance.index()] > 0
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "both admission classes must remain queued; waiting={waiting:?}"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn root_labelled_permits_name_the_root_without_sharing_a_slot() {
        let limiter = test_limiter(4);
        let root = std::path::Path::new("/work/labelled-root");
        let first = acquire_blocking_while_for_root_with_limiter(&limiter, "first", root, || true)
            .expect("first labelled permit");
        let second =
            acquire_blocking_while_for_root_with_limiter(&limiter, "second", root, || true)
                .expect("second labelled permit");

        let census = limiter.census();
        assert_eq!(census.holders.len(), 2, "{census:?}");
        assert!(census
            .holders
            .iter()
            .all(|holder| holder.root == root.display().to_string() && holder.sharers == 1));
        assert_eq!(
            limiter.available.load(Ordering::Acquire),
            2,
            "a root label must not merge two builds into one shared slot"
        );
        drop((first, second));
        assert_eq!(limiter.available.load(Ordering::Acquire), 4);
    }

    #[test]
    fn permits_release_on_drop() {
        let _serial = serial();
        let before = GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire);
        {
            let _a = acquire_blocking("test-a");
            let _b = acquire_blocking("test-b");
            assert_eq!(
                GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire),
                before - 2
            );
        }
        assert_eq!(
            GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire),
            before
        );
    }

    #[test]
    fn acquire_blocking_waits_until_release() {
        let _serial = serial();
        // Drain every slot, then prove a waiter blocks until one holder drops.
        let mut held: Vec<ColdBuildPermit> = Vec::new();
        while let Some(permit) = try_acquire() {
            held.push(permit);
        }
        let waiter = std::thread::spawn(|| {
            let _p = acquire_blocking("waiter");
        });
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!waiter.is_finished(), "waiter must block while cap is full");
        drop(held.pop());
        waiter.join().expect("waiter finishes after release");
        drop(held);
    }

    #[test]
    fn admission_revoked_between_check_and_permit_drops_the_slot() {
        let _serial = serial();
        let before = GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire);
        let checks = AtomicUsize::new(0);

        let permit = acquire_blocking_while("revoked-after-cas", || {
            checks.fetch_add(1, Ordering::SeqCst) == 0
        });

        assert!(permit.is_none());
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert_eq!(
            GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire),
            before,
            "revoked admission must return the just-acquired slot"
        );
    }

    #[test]
    fn conditional_waiter_cancels_without_consuming_a_released_slot() {
        let _serial = serial();
        let mut held = Vec::new();
        while let Some(permit) = try_acquire() {
            held.push(permit);
        }
        let admitted = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let waiter_admitted = Arc::clone(&admitted);
        let waiter = std::thread::spawn(move || {
            acquire_blocking_while("conditional waiter", || {
                waiter_admitted.load(Ordering::SeqCst)
            })
        });
        std::thread::sleep(Duration::from_millis(150));
        admitted.store(false, Ordering::SeqCst);
        assert!(
            waiter.join().expect("conditional waiter joins").is_none(),
            "revoked work must leave the cold-build queue without taking a permit"
        );
        drop(held);
    }

    #[test]
    fn cancellation_after_acquisition_returns_the_permit_without_an_event() {
        let limiter = test_limiter(1);
        let cancellation_checks = AtomicUsize::new(0);

        let permit = acquire_blocking_while_cancellable_with_limiter(
            &limiter,
            "cancel-after-acquire",
            ColdBuildAdmissionRequest::new(
                "inspect-cancelled",
                ColdBuildAdmissionClass::InspectTriggered,
            ),
            || true,
            || cancellation_checks.fetch_add(1, Ordering::SeqCst) > 0,
        );

        assert!(permit.is_none());
        assert_eq!(cancellation_checks.load(Ordering::SeqCst), 2);
        assert_eq!(
            limiter.available.load(Ordering::Acquire),
            1,
            "post-acquisition cancellation must return the permit"
        );
        assert!(
            limiter.admission_events().is_empty(),
            "cancelled work must not emit a successful admission"
        );
    }

    #[test]
    fn standing_yields_before_initial_and_checkpoint_reacquisition_when_non_standing_waits() {
        let limiter = test_limiter(1);
        let maintenance = ColdBuildAdmissionRequest::new(
            "maintenance-waiter",
            ColdBuildAdmissionClass::Maintenance,
        );
        let non_standing_waiter =
            AdmissionWaiter::register(&limiter, &maintenance, "maintenance waiter");

        assert!(acquire_standing_while_cancellable_with_limiter(
            &limiter,
            "standing-initial",
            "standing-initial",
            41,
            || true,
            || false,
        )
        .is_none());

        drop(non_standing_waiter);
        let first = acquire_standing_while_cancellable_with_limiter(
            &limiter,
            "standing-checkpoint",
            "standing-checkpoint",
            41,
            || true,
            || false,
        )
        .expect("standing may acquire once ordinary waiters clear");
        assert_eq!(first.admission_epoch, 41);
        drop(first);

        let inspect = ColdBuildAdmissionRequest::new(
            "inspect-waiter",
            ColdBuildAdmissionClass::InspectTriggered,
        );
        let non_standing_waiter = AdmissionWaiter::register(&limiter, &inspect, "inspect waiter");
        assert!(acquire_standing_while_cancellable_with_limiter(
            &limiter,
            "standing-reacquire",
            "standing-reacquire",
            41,
            || true,
            || false,
        )
        .is_none());
        drop(non_standing_waiter);
    }

    #[test]
    fn inspect_waiter_takes_next_release_ahead_of_queued_maintenance() {
        let limiter = test_limiter(1);
        let active_request = ColdBuildAdmissionRequest::new(
            "active-semantic-seed",
            ColdBuildAdmissionClass::Maintenance,
        );
        let active = try_acquire_classified_with_limiter(&limiter, &active_request)
            .expect("active maintenance build holds the slot");
        let (admitted_tx, admitted_rx) = std::sync::mpsc::channel();
        let mut waiters = Vec::new();

        for (request_id, class) in [
            ("queued-refresh", ColdBuildAdmissionClass::Maintenance),
            (
                "blocking-inspect",
                ColdBuildAdmissionClass::InspectTriggered,
            ),
        ] {
            let limiter = Arc::clone(&limiter);
            let admitted_tx = admitted_tx.clone();
            waiters.push(std::thread::spawn(move || {
                let permit = acquire_blocking_while_cancellable_with_limiter(
                    &limiter,
                    request_id,
                    ColdBuildAdmissionRequest::new(request_id, class),
                    || true,
                    || false,
                )
                .expect("queued build is admitted");
                admitted_tx
                    .send((class, permit))
                    .expect("test receives admitted permit");
            }));
        }
        drop(admitted_tx);
        wait_for_waiters(&limiter);
        drop(active);

        let (first_class, first_permit) = admitted_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("released slot admits interactive inspect");
        assert_eq!(first_class, ColdBuildAdmissionClass::InspectTriggered);
        assert!(
            admitted_rx.try_recv().is_err(),
            "maintenance remains deferred"
        );
        drop(first_permit);

        let (second_class, second_permit) = admitted_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("maintenance resumes after inspect releases its slot");
        assert_eq!(second_class, ColdBuildAdmissionClass::Maintenance);
        drop(second_permit);
        for waiter in waiters {
            waiter.join().expect("admission waiter joins");
        }
    }

    #[test]
    fn same_root_requests_share_one_slot_and_last_sharer_releases_it() {
        let limiter = test_limiter(2);
        let first = try_acquire_classified_with_limiter(
            &limiter,
            &ColdBuildAdmissionRequest::for_root(
                "/project/a",
                "maintenance-a",
                ColdBuildAdmissionClass::Maintenance,
            ),
        )
        .expect("first root acquires one slot");
        let second = try_acquire_classified_with_limiter(
            &limiter,
            &ColdBuildAdmissionRequest::for_root(
                "/project/a",
                "inspect-a",
                ColdBuildAdmissionClass::InspectTriggered,
            ),
        )
        .expect("same root shares its existing slot across classes");
        let third = try_acquire_classified_with_limiter(
            &limiter,
            &ColdBuildAdmissionRequest::for_root(
                "/project/b",
                "inspect-b",
                ColdBuildAdmissionClass::InspectTriggered,
            ),
        )
        .expect("another root can use the fleet's second slot");

        assert_eq!(limiter.available.load(Ordering::Acquire), 0);
        let census = limiter.census();
        assert_eq!(census.holders.len(), 2);
        assert_eq!(
            census
                .holders
                .iter()
                .find(|holder| holder.root == "/project/a")
                .map(|holder| holder.sharers),
            Some(2)
        );
        assert_eq!(
            limiter.admission_events().len(),
            2,
            "sharing an existing root permit is not a second slot acquisition"
        );

        drop(first);
        assert_eq!(limiter.available.load(Ordering::Acquire), 0);
        assert_eq!(
            limiter.census().holders[0].sharers,
            1,
            "the first drop leaves the other same-root sharer admitted"
        );
        drop(second);
        assert_eq!(limiter.available.load(Ordering::Acquire), 1);
        drop(third);
        assert_eq!(limiter.available.load(Ordering::Acquire), 2);
    }

    #[test]
    fn cancelling_one_same_root_sharer_does_not_release_the_other() {
        let limiter = test_limiter(1);
        let remaining = try_acquire_classified_with_limiter(
            &limiter,
            &ColdBuildAdmissionRequest::for_root(
                "/project/shared",
                "remaining",
                ColdBuildAdmissionClass::Maintenance,
            ),
        )
        .expect("remaining work acquires the root slot");
        let cancelled = try_acquire_classified_with_limiter(
            &limiter,
            &ColdBuildAdmissionRequest::for_root(
                "/project/shared",
                "cancelled",
                ColdBuildAdmissionClass::InspectTriggered,
            ),
        )
        .expect("cancellable work shares the root slot");

        drop(cancelled);
        assert_eq!(limiter.available.load(Ordering::Acquire), 0);
        let holder = limiter.census().holders.pop().expect("remaining holder");
        assert_eq!(holder.root, "/project/shared");
        assert_eq!(holder.sharers, 1);

        drop(remaining);
        assert_eq!(limiter.available.load(Ordering::Acquire), 1);
        assert!(limiter.census().holders.is_empty());
    }

    #[test]
    fn admission_events_cover_both_classes_across_the_fixed_32_release_schedule() {
        const RELEASE_COUNT: usize = 32;

        let limiter = test_limiter(1);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (permit_tx, permit_rx) = std::sync::mpsc::channel();
        let initial_permit = limiter.try_acquire().expect("hold the only slot");
        let mut waiters = Vec::new();

        for (request_id, class) in [
            ("inspect-request", ColdBuildAdmissionClass::InspectTriggered),
            ("maintenance-request", ColdBuildAdmissionClass::Maintenance),
        ] {
            let limiter = Arc::clone(&limiter);
            let cancelled = Arc::clone(&cancelled);
            let permit_tx = permit_tx.clone();
            waiters.push(std::thread::spawn(move || {
                while !cancelled.load(Ordering::SeqCst) {
                    let permit = acquire_blocking_while_cancellable_with_limiter(
                        &limiter,
                        "fixed-release-test",
                        ColdBuildAdmissionRequest::new(request_id, class),
                        || true,
                        || cancelled.load(Ordering::SeqCst),
                    );
                    let Some(permit) = permit else {
                        return;
                    };
                    if permit_tx.send(permit).is_err() {
                        return;
                    }
                }
            }));
        }
        drop(permit_tx);

        wait_for_waiters(&limiter);
        let mut released_permit = Some(initial_permit);
        for release in 1..RELEASE_COUNT {
            wait_for_waiters(&limiter);
            drop(released_permit.take());
            released_permit = Some(
                permit_rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap_or_else(|error| {
                        panic!("release {release} must admit a waiter: {error}")
                    }),
            );
        }
        wait_for_waiters(&limiter);
        drop(released_permit);
        let consumed_by_build = permit_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or_else(|error| panic!("release {RELEASE_COUNT} must admit a waiter: {error}"));

        cancelled.store(true, Ordering::SeqCst);
        for waiter in waiters {
            waiter.join().expect("cancelled waiter joins");
        }

        let events = limiter.admission_events();
        assert_eq!(events.len(), RELEASE_COUNT);
        assert!(events
            .iter()
            .any(|event| event.class == ColdBuildAdmissionClass::InspectTriggered));
        assert!(events
            .iter()
            .any(|event| event.class == ColdBuildAdmissionClass::Maintenance));
        assert!(events.iter().all(|event| matches!(
            event.request_id.as_str(),
            "inspect-request" | "maintenance-request"
        )));
        assert!(events
            .iter()
            .enumerate()
            .all(|(index, event)| event.admission_order == index as u64 + 1));

        assert_eq!(
            limiter.available.load(Ordering::Acquire),
            0,
            "the final acquired permit must remain accounted for by the consumed build"
        );
        drop(consumed_by_build);
        assert_eq!(
            limiter.available.load(Ordering::Acquire),
            1,
            "releasing the consumed build permit must restore the limiter slot"
        );
    }
}

#[cfg(test)]
mod census_tests {
    use super::*;

    #[test]
    fn census_names_holders_and_queued_requests_without_holding_work_locks() {
        let limiter = isolated_limiter(1);
        let permit = acquire_blocking_while_cancellable_with_limiter(
            &limiter,
            "explicit inspect Tier-2 run",
            ColdBuildAdmissionRequest::new(
                "inspect:/tmp/project:1",
                ColdBuildAdmissionClass::InspectTriggered,
            ),
            || true,
            || false,
        )
        .expect("first permit");
        let holder = limiter.census().holders.pop().expect("holder census");
        assert_eq!(holder.domain, "inspect-triggered");
        assert_eq!(holder.root, "/tmp/project");
        assert_eq!(holder.sharers, 1);
        assert_eq!(holder.kind, "explicit inspect Tier-2 run");

        let waiter_limiter = Arc::clone(&limiter);
        let waiter = std::thread::spawn(move || {
            acquire_blocking_while_cancellable_with_limiter(
                &waiter_limiter,
                "queued background refresh",
                ColdBuildAdmissionRequest::new(
                    "inspect:/tmp/queued:2",
                    ColdBuildAdmissionClass::Maintenance,
                ),
                || true,
                || false,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while limiter.census().queued.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let queued = limiter.census().queued.pop().expect("queued census");
        assert_eq!(queued.root, "/tmp/queued");
        assert_eq!(queued.sharers, 1);
        drop(permit);
        drop(
            waiter
                .join()
                .expect("waiter thread")
                .expect("queued permit"),
        );
        assert!(limiter.census().holders.is_empty());
        assert!(limiter.census().queued.is_empty());
    }
}
