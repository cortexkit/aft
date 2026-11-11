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
pub(crate) struct SubcLifecycleAdmission {
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

    #[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn cancellation_hint_preserves_grace_and_rebind_decisions() {
        let admission = SubcLifecycleAdmission::default();
        let worker = admission.clone();
        let generation = AtomicU64::new(0);
        let cancelled = std::cell::Cell::new(false);
        worker.cancel_if_abandoned(|| cancelled.set(true));
        assert!(!cancelled.get(), "bound roots must keep running");

        admission.set_abandon_grace_for_test(Duration::from_secs(600));
        admission.mark_unbound(&generation);
        worker.cancel_if_abandoned(|| cancelled.set(true));
        assert!(
            !cancelled.get(),
            "a short unbind must preserve the grace window"
        );

        // Expire the grace window without sleeping or advancing the unrelated
        // filesystem probe. A shared lifecycle sees expiry at its next check.
        *admission.unbound_since.lock() = Some(Instant::now() - Duration::from_secs(601));
        worker.cancel_if_abandoned(|| cancelled.set(true));
        assert!(
            cancelled.replace(false),
            "expired unbound roots must cancel"
        );

        admission.mark_bound();
        worker.cancel_if_abandoned(|| cancelled.set(true));
        assert!(!cancelled.get(), "rebind must clear the cancellation hint");

        admission.set_abandon_grace_for_test(Duration::ZERO);
        admission.mark_unbound(&generation);
        worker.cancel_if_abandoned(|| cancelled.set(true));
        assert!(cancelled.get(), "a second unbind must publish a fresh hint");
    }

    #[test]
    fn cancellation_hint_never_overrides_synchronized_rebind() {
        let admission = SubcLifecycleAdmission::default();
        let generation = AtomicU64::new(0);
        admission.set_abandon_grace_for_test(Duration::ZERO);
        admission.mark_unbound(&generation);
        admission.mark_bound();
        // A worker may have loaded the previous true hint just before rebind.
        // The admission lock's bound state remains the authoritative decision.
        admission.unbound_hint.store(true, Ordering::Release);
        let cancelled = std::cell::Cell::new(false);
        admission.cancel_if_abandoned(|| cancelled.set(true));
        assert!(
            !cancelled.get(),
            "a stale hint must not cancel a rebound root"
        );
    }

    #[test]
    fn lifecycle_gate_serializes_unbind_with_worker_start_commit() {
        let admission = SubcLifecycleAdmission::default();
        let generation = Arc::new(AtomicU64::new(11));
        let expected = generation.load(Ordering::SeqCst);
        let starts = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let worker_admission = admission.clone();
        let worker_generation = Arc::clone(&generation);
        let worker_starts = Arc::clone(&starts);
        let worker = std::thread::spawn(move || {
            worker_admission.run_if_current(&worker_generation, expected, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                worker_starts.fetch_add(1, Ordering::SeqCst);
            })
        });
        entered_rx.recv().unwrap();

        let unbind_admission = admission.clone();
        let unbind_generation = Arc::clone(&generation);
        let (unbound_tx, unbound_rx) = std::sync::mpsc::channel();
        let unbind = std::thread::spawn(move || {
            unbind_admission.mark_unbound(&unbind_generation);
            unbound_tx.send(()).unwrap();
        });

        assert!(
            unbound_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "unbind must wait for an admitted worker-start commit"
        );
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_some());
        unbound_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        unbind.join().unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(
            admission
                .run_if_current(&generation, generation.load(Ordering::SeqCst), || {
                    starts.fetch_add(1, Ordering::SeqCst);
                })
                .is_none(),
            "worker starts after unbind must be denied"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn lifecycle_gate_recursive_acquisition_panics_instead_of_parking() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let admission = SubcLifecycleAdmission::default();
            let generation = AtomicU64::new(11);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                admission.run_if_current(&generation, 11, || {
                    // A clone must not bypass the same-thread ownership marker.
                    admission.clone().is_bound();
                });
            }));
            tx.send(result).unwrap();
        });
        let result = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("recursive acquisition must panic before parking on the lifecycle mutex");
        worker.join().unwrap();
        let panic = result.expect_err("recursive acquisition was accepted");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied());
        assert_eq!(message, Some("recursive root lifecycle lock acquisition"));
    }

    #[test]
    fn lifecycle_gate_marker_allows_other_roots_and_cleans_up_after_unwind() {
        let admission = SubcLifecycleAdmission::default();
        let other = SubcLifecycleAdmission::default();
        let generation = AtomicU64::new(11);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admission.run_if_current(&generation, 11, || {
                assert!(other.is_bound());
                assert_eq!(admission.try_is_bound(), None);
                panic!("unwind admission");
            });
        }));
        assert!(result.is_err());
        assert!(admission.is_bound());
    }
    #[test]
    fn unbound_lifecycle_rejects_all_deferred_worker_starts() {
        let admission = SubcLifecycleAdmission::default();
        let generation = Arc::new(AtomicU64::new(7));
        admission.mark_unbound(&generation);
        let expected = generation.load(Ordering::SeqCst);
        let starts = Arc::new(AtomicUsize::new(0));

        let workers = (0..16)
            .map(|_| {
                let admission = admission.clone();
                let generation = Arc::clone(&generation);
                let starts = Arc::clone(&starts);
                std::thread::spawn(move || {
                    admission.run_if_current(&generation, expected, || {
                        starts.fetch_add(1, Ordering::SeqCst);
                    })
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            assert!(worker.join().unwrap().is_none());
        }
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }
}
