use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

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
    class: ColdBuildAdmissionClass,
}

impl ColdBuildAdmissionRequest {
    pub(crate) fn new(request_id: impl Into<String>, class: ColdBuildAdmissionClass) -> Self {
        Self {
            request_id: request_id.into(),
            class,
        }
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

/// Attempt immediate admission without bypassing queued requests from the other
/// class. Background schedulers use this when they can defer rejected work.
pub(crate) fn try_acquire_classified_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    request: &ColdBuildAdmissionRequest,
) -> Option<ColdBuildPermit> {
    limiter.try_acquire_classified(request, || true)
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

/// Immediate standing admission without waiter registration, preserving the
/// lifecycle admission epoch. Used by standing passes that can defer rejected
/// work to their next tick: a yielded pass never occupies a worker waiting for
/// a cold slot. No equivalent epoch-preserving immediate API existed before.
pub(crate) fn try_acquire_standing_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    request_id: impl Into<String>,
    admission_epoch: u64,
) -> Option<StandingColdBuildPermit> {
    // Mirror the blocking path's yield: standing try-admission defers the
    // final slot to any already-queued interactive or ordinary maintenance
    // waiter instead of jumping the queue.
    if limiter.has_non_standing_waiters() {
        return None;
    }
    let request = ColdBuildAdmissionRequest::new(request_id, ColdBuildAdmissionClass::Standing);
    try_acquire_classified_with_limiter(limiter, &request).map(|permit| StandingColdBuildPermit {
        _permit: permit,
        admission_epoch,
    })
}

fn acquire_blocking_while_inner(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    request: Option<&ColdBuildAdmissionRequest>,
    admitted: impl Fn() -> bool,
    cancelled: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    let _waiter = request.map(|request| AdmissionWaiter::register(limiter, request.class));
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
            Some(request) => limiter.try_acquire_classified(request, || {
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
            if logged {
                match request {
                    Some(request) => crate::slog_info!(
                        "{} cold-build slot acquired after {}ms wait: request={} kind={}",
                        request.class.label(),
                        started.elapsed().as_millis(),
                        request.request_id,
                        kind
                    ),
                    None => crate::slog_info!(
                        "maintenance build slot acquired after {}ms wait: {}",
                        started.elapsed().as_millis(),
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
    events: VecDeque<ColdBuildAdmissionEvent>,
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
                events: VecDeque::with_capacity(ADMISSION_EVENT_RETENTION),
            }),
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ColdBuildPermit> {
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
                return Some(ColdBuildPermit {
                    limiter: Arc::clone(self),
                });
            }
        }
    }

    fn try_acquire_classified(
        self: &Arc<Self>,
        request: &ColdBuildAdmissionRequest,
        admitted_after_acquire: impl FnOnce() -> bool,
    ) -> Option<ColdBuildPermit> {
        let mut state = self
            .admission_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let permit = self.try_acquire()?;
        if !admitted_after_acquire() {
            drop(permit);
            return None;
        }
        Self::record_admission_locked(&mut state, request);
        Some(permit)
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
}

impl AdmissionWaiter {
    fn register(limiter: &Arc<ColdBuildLimiter>, class: ColdBuildAdmissionClass) -> Self {
        limiter.waiting_by_class[class.index()].fetch_add(1, Ordering::AcqRel);
        Self {
            limiter: Arc::clone(limiter),
            class,
        }
    }
}

impl Drop for AdmissionWaiter {
    fn drop(&mut self) {
        let previous =
            self.limiter.waiting_by_class[self.class.index()].fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

#[derive(Debug)]
pub struct ColdBuildPermit {
    limiter: Arc<ColdBuildLimiter>,
}

impl Drop for ColdBuildPermit {
    fn drop(&mut self) {
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

    #[test]
    fn standing_try_admission_yields_to_non_standing_waiters() {
        let limiter = test_limiter(1);
        let holder = limiter.try_acquire().expect("hold the only slot");
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Queue a non-standing waiter behind the held slot.
        let waiter_limiter = Arc::clone(&limiter);
        let waiter_cancel = Arc::clone(&cancelled);
        let waiter = std::thread::spawn(move || {
            let _permit = acquire_blocking_while_cancellable_with_limiter(
                &waiter_limiter,
                "yield-test",
                ColdBuildAdmissionRequest::new("maintenance", ColdBuildAdmissionClass::Maintenance),
                || true,
                || waiter_cancel.load(Ordering::SeqCst),
            );
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !limiter.has_non_standing_waiters() {
            assert!(
                Instant::now() < deadline,
                "maintenance waiter must register"
            );
            std::thread::yield_now();
        }
        // Standing try-admission must yield while the waiter is queued.
        assert!(
            try_acquire_standing_with_limiter(&limiter, "standing-yield", 0).is_none(),
            "standing try-admission must defer to queued non-standing waiters"
        );
        cancelled.store(true, Ordering::SeqCst);
        drop(holder);
        waiter.join().expect("waiter exits after cancellation");
    }

    #[test]
    fn standing_try_admission_takes_slot_without_waiters() {
        let limiter = test_limiter(1);
        let permit = try_acquire_standing_with_limiter(&limiter, "standing-free", 7);
        assert!(
            permit.is_some(),
            "standing takes a free slot with no waiters"
        );
        assert_eq!(
            limiter.available.load(Ordering::Acquire),
            0,
            "standing permit occupies the slot"
        );
    }
}
