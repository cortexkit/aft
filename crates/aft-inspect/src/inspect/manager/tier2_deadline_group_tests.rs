#[cfg(test)]
mod tier2_deadline_tests {
    use super::*;

    #[test]
    fn tier2_pass_deadline_releases_limiter_while_ignoring_stub_still_runs() {
        let limiter = cold_build_limiter::isolated_limiter(1);
        let request = cold_build_limiter::ColdBuildAdmissionRequest::new(
            "tier2-timeout-test",
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let permit = cold_build_limiter::try_acquire_classified_with_limiter(&limiter, &request)
            .expect("test permit");
        let permit_slot = Arc::new(Mutex::new(Some(permit)));
        let (running_tx, running_rx) = bounded(1);
        let (release_tx, release_rx) = bounded::<()>(1);
        let root = tempfile::tempdir().expect("isolated deadline-test root");
        let handle = std::thread::spawn({
            let permit_slot = Arc::clone(&permit_slot);
            move || {
                run_tier2_pass_with_deadline(
                    root.path(),
                    InspectCategory::Duplicates,
                    Duration::from_millis(20),
                    Some(permit_slot),
                    || {
                        running_tx.send(()).expect("announce running stub");
                        // Keep the producer alive until the test has observed permit
                        // release, even if its assertion thread is descheduled.
                        let _ = release_rx.recv();
                        "ignored cancellation"
                    },
                )
            }
        });
        running_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("stub started");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !limiter.census().holders.is_empty() {
            assert!(
                Instant::now() < deadline,
                "deadline did not release limiter slot"
            );
            std::thread::yield_now();
        }
        assert!(
            !handle.is_finished(),
            "stub must still be running when slot is released"
        );
        release_tx.send(()).expect("release ignoring stub");
        let (value, timed_out) = handle.join().expect("stub thread");
        assert!(timed_out);
        assert_eq!(value, "ignored cancellation");
    }

    #[test]
    fn tier2_pass_deadline_cooperative_stub_observes_cancellation() {
        let limiter = cold_build_limiter::isolated_limiter(1);
        let permit = limiter.try_acquire().expect("test permit");
        let root = tempfile::tempdir().expect("existing deadline-test root");
        let (value, timed_out) = run_tier2_pass_with_deadline(
            root.path(),
            InspectCategory::Duplicates,
            Duration::from_millis(20),
            Some(Arc::new(Mutex::new(Some(permit)))),
            || {
                let hang_ceiling = Instant::now() + Duration::from_secs(30);
                while !crate::executor::current_job_cancelled() {
                    assert!(Instant::now() < hang_ceiling, "stub was never cancelled");
                    std::thread::sleep(Duration::from_millis(1));
                }
                "cancelled"
            },
        );
        assert!(timed_out, "deadline must own the cancellation request");
        assert_eq!(value, "cancelled");
        assert!(limiter.census().holders.is_empty());
    }
}
