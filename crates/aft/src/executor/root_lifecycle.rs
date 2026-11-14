pub use aft_base::executor::root_lifecycle::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, Instant};

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
