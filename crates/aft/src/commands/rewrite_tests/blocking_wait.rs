mod tests {
    use std::time::{Duration, Instant};

    use crate::blocking_wait::{clamp_worker_wait_ms, ReplyBudget};

    #[tokio::test(start_paused = true)]
    async fn runtime_clock_drives_reply_expiry_and_combines_with_suspend() {
        use futures_util::{poll, FutureExt};
        use std::sync::atomic::{AtomicU64, Ordering};
        let receipt = tokio::time::Instant::now().into_std();
        let suspend = Box::leak(Box::new(AtomicU64::new(0)));
        let budget =
            ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(3))).with_runtime_clock();
        let suspend_budget = budget.with_suspend_clock(suspend);
        assert_eq!(budget.deadline, receipt + Duration::from_millis(1_500));
        let wait = budget.sleep_until(budget.deadline);
        tokio::pin!(wait);
        assert!(poll!(wait.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(1_499)).await;
        assert_eq!(budget.elapsed(), Duration::from_millis(1_499));
        assert!(poll!(wait.as_mut()).is_pending());
        suspend.store(1, Ordering::SeqCst);
        assert!(suspend_budget
            .sleep_until(budget.deadline)
            .now_or_never()
            .is_some());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(poll!(wait.as_mut()).is_ready());
        assert_eq!(budget.elapsed(), Duration::from_millis(1_500));
    }

    #[tokio::test]
    async fn suspend_makes_wait_and_reply_budget_due_at_resume() {
        use futures_util::FutureExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        let suspend = Box::leak(Box::new(AtomicU64::new(0)));
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(280)))
            .with_suspend_clock(suspend);
        let wait_deadline = budget.cap(receipt + Duration::from_secs(270));
        suspend.store(2_000_000, Ordering::SeqCst);
        assert!(
            Instant::now() < wait_deadline,
            "awake clock did not reach the wait limit"
        );
        assert!(budget.now() >= budget.deadline);
        assert!(budget.elapsed() >= Duration::from_secs(2_000));
        // Poll once at resume: no further awake-time scheduler tick is needed.
        assert!(budget.sleep_until(wait_deadline).now_or_never().is_some());
    }

    #[test]
    fn launch_restore_wait_refunds_startup_but_not_caller_deadline() {
        let receipt = Instant::now() - Duration::from_secs(20);
        let budget = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(2)));
        assert!(budget.deadline < Instant::now());
        let shifted = budget.after_launch_restore(Duration::from_secs(20));
        assert!(shifted.deadline < Instant::now());
        assert_eq!(
            shifted.received_at - budget.received_at,
            Duration::from_secs(20)
        );
        assert_eq!(shifted.deadline, budget.deadline);
        assert_eq!(
            budget.after_launch_restore(Duration::ZERO).deadline,
            budget.deadline
        );
    }

    #[test]
    fn default_worker_wait_keeps_full_thirty_minutes() {
        let configured = crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS;
        assert_eq!(
            crate::commands::bash_orchestrate::blocking_wait_cap_ms(true, configured),
            Some(1_800_000),
        );
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, None);
        let ordinary_wait = receipt + Duration::from_millis(configured);
        assert_eq!(budget.cap(ordinary_wait), ordinary_wait);
        assert_eq!(budget.deadline - receipt, Duration::from_millis(1_836_000));
    }

    #[test]
    fn older_admitted_client_window_still_has_headroom() {
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(1_830)));
        let ordinary_wait = receipt + Duration::from_secs(1_800);
        assert_eq!(
            budget.cap(ordinary_wait) - receipt,
            Duration::from_secs(1_647)
        );
    }

    #[test]
    fn no_client_deadline_clamps_even_oversized_worker_limits() {
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, None);
        assert_eq!(budget.deadline - receipt, Duration::from_millis(1_836_000));
        assert_eq!(clamp_worker_wait_ms(u64::MAX), 1_836_000);
        assert_eq!(clamp_worker_wait_ms(60_000), 60_000);
    }

    #[test]
    fn queue_time_consumes_the_original_reply_window() {
        let receipt = Instant::now();
        let client = receipt + Duration::from_secs(2_040);
        let budget = ReplyBudget::new(receipt, Some(client));
        let queued_for = Duration::from_secs(120);
        assert_eq!(
            budget.deadline - (receipt + queued_for),
            Duration::from_secs(1_716)
        );
        assert_eq!(client - budget.deadline, Duration::from_secs(204));
        let short = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(10)));
        assert_eq!(short.deadline - receipt, Duration::from_secs(5));
    }

    #[test]
    fn client_deadline_keeps_reader_queue_time() {
        let receipt = Instant::now();
        let body = serde_json::json!({"deadline_ms": 20_000});
        let unqueued = ReplyBudget::from_clocks(receipt, &body, receipt, 10_000);
        let queue_time = Duration::from_secs(4);
        let dispatched = receipt + queue_time;
        let queued = ReplyBudget::from_clocks(receipt, &body, dispatched, 14_000);
        assert_eq!(unqueued.deadline, queued.deadline);
        assert_eq!(unqueued.deadline - receipt, Duration::from_secs(5));
        assert_eq!(queued.deadline - dispatched, Duration::from_secs(1));
        assert_eq!(
            (unqueued.deadline - receipt) - (queued.deadline - dispatched),
            queue_time
        );
    }
}
