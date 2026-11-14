use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a root may stay unbound before its long-running cold builds
/// (semantic embedding, callgraph extraction) are abandoned. A host restart
/// unbinds every root and rebinds it within seconds; a rebind inside this
/// window leaves in-flight builds untouched. A root that stays unbound past
/// the window has really been abandoned, and its builds stop at their next
/// batch or slice boundary so they release the process-wide cold-build slot
/// and stop loading the embedding backend.
pub(crate) const UNBOUND_BUILD_ABANDON_GRACE: Duration = Duration::from_secs(120);

#[cfg(debug_assertions)]
thread_local! {
    static HELD_LIFECYCLE_LOCKS: std::cell::RefCell<Vec<usize>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// Detect recursive admission before parking on a non-reentrant mutex. Track
/// the mutex identity, not the context, because cloned admissions share it.
struct LifecycleLockMarker {
    #[cfg(debug_assertions)]
    identity: usize,
}

impl LifecycleLockMarker {
    fn enter(_mutex: &Arc<parking_lot::Mutex<bool>>) -> Self {
        #[cfg(debug_assertions)]
        {
            let identity = Arc::as_ptr(_mutex) as usize;
            HELD_LIFECYCLE_LOCKS.with(|held| {
                let mut held = held.borrow_mut();
                assert!(
                    !held.contains(&identity),
                    "recursive root lifecycle lock acquisition"
                );
                held.push(identity);
            });
            Self { identity }
        }
        #[cfg(not(debug_assertions))]
        Self {}
    }
}

impl Drop for LifecycleLockMarker {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        HELD_LIFECYCLE_LOCKS.with(|held| {
            let mut held = held.borrow_mut();
            let position = held.iter().rposition(|id| *id == self.identity).unwrap();
            held.remove(position);
        });
    }
}

/// Serializes the daemon's bound/unbound transition with admission of deferred
/// root work. The lock covers only the bounded decision and worker-start commit;
/// call sites must not wait for worker completion or run a scan while holding it.
#[derive(Clone, Debug)]
pub struct SubcLifecycleAdmission {
    pub(crate) unbound: Arc<parking_lot::Mutex<bool>>,
    /// Mirrors `unbound` for cancellation checkpoints. A bound root takes the
    /// atomic fast path; an unbound root still confirms grace and rebind under
    /// the admission lock before signalling cancellation.
    unbound_hint: Arc<AtomicBool>,
    /// When the current unbound period started; `None` while bound. Written
    /// only while holding `unbound`.
    unbound_since: Arc<parking_lot::Mutex<Option<Instant>>>,
    /// Grace window for `unbound_past_grace`, in milliseconds. Per lifecycle
    /// so a test can shorten it without affecting other contexts.
    abandon_grace_ms: Arc<AtomicU64>,
}

impl Default for SubcLifecycleAdmission {
    fn default() -> Self {
        Self {
            unbound: Arc::default(),
            unbound_hint: Arc::default(),
            unbound_since: Arc::default(),
            abandon_grace_ms: Arc::new(AtomicU64::new(
                UNBOUND_BUILD_ABANDON_GRACE.as_millis() as u64
            )),
        }
    }
}

impl SubcLifecycleAdmission {
    pub(crate) fn mark_bound(&self) {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let mut unbound = self.unbound.lock();
        *unbound = false;
        *self.unbound_since.lock() = None;
        self.unbound_hint.store(false, Ordering::Release);
    }

    pub(crate) fn mark_unbound(&self, configure_generation: &AtomicU64) {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let mut unbound = self.unbound.lock();
        if !*unbound {
            *unbound = true;
            *self.unbound_since.lock() = Some(Instant::now());
            configure_generation.fetch_add(1, Ordering::SeqCst);
            self.unbound_hint.store(true, Ordering::Release);
        }
    }

    /// True when the root has had no bound route for longer than the abandon
    /// grace window. Long cold builds poll this between batches: a transient
    /// unbind (rebind within the window) never trips it, so those builds keep
    /// running exactly as they do while bound.
    pub(crate) fn unbound_past_grace(&self) -> bool {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let unbound = self.unbound.lock();
        if !*unbound {
            return false;
        }
        let grace = Duration::from_millis(self.abandon_grace_ms.load(Ordering::SeqCst));
        self.unbound_since
            .lock()
            .is_some_and(|since| since.elapsed() >= grace)
    }

    /// Serialize cancellation with rebind: a root rebound before this decision
    /// cannot have its still-running work cancelled by a stale grace check.
    pub(crate) fn cancel_if_abandoned(&self, cancel: impl FnOnce()) {
        // Lifecycle transitions are in memory and must be observed at every
        // parser checkpoint, independently of the throttled disk-liveness
        // probe. Avoid a mutex on the usual bound-root path.
        if !self.unbound_hint.load(Ordering::Acquire) {
            return;
        }
        // A checkpoint may be inside a lifecycle admission closure that already
        // owns this lock. Defer that checkpoint instead of recursively locking;
        // the next batch/parser poll makes the synchronized decision.
        let Some(unbound) = self.unbound.try_lock() else {
            return;
        };
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let grace = Duration::from_millis(self.abandon_grace_ms.load(Ordering::SeqCst));
        if *unbound
            && self
                .unbound_since
                .lock()
                .is_some_and(|since| since.elapsed() >= grace)
        {
            cancel();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_abandon_grace_for_test(&self, grace: Duration) {
        self.abandon_grace_ms
            .store(grace.as_millis() as u64, Ordering::SeqCst);
    }

    pub(crate) fn advance_generation(&self, generation: &AtomicU64) -> u64 {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let _unbound = self.unbound.lock();
        generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    pub(crate) fn run_if_current<R>(
        &self,
        generation: &AtomicU64,
        expected: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let unbound = self.unbound.lock();
        if *unbound || generation.load(Ordering::SeqCst) != expected {
            return None;
        }
        Some(action())
    }

    pub(crate) fn is_bound(&self) -> bool {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        !*self.unbound.lock()
    }

    pub(crate) fn try_is_bound(&self) -> Option<bool> {
        self.unbound.try_lock().map(|unbound| !*unbound)
    }

    pub(crate) fn is_unbound(&self) -> bool {
        !self.is_bound()
    }

    pub(crate) fn run_if_unbound<R>(&self, action: impl FnOnce() -> R) -> Option<R> {
        let _held = LifecycleLockMarker::enter(&self.unbound);
        let unbound = self.unbound.lock();
        if !*unbound {
            return None;
        }
        Some(action())
    }
}
