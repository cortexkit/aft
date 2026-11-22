//! Cheap cached liveness reads wake expensive diagnostics only when needed.

use super::*;

// aft.status reads the published lifecycle census, and diagnostics.findings
// reads recorded health evidence without scheduling work. Neither has a 3 s
// refresh contract. Diagnostic alerts and the response status line use their
// own observations, not this rollup. Keep these two cache consumers supplied
// slowly; HealthCheck and memory.census wake the worker on demand.
pub(super) const BACKGROUND_INTERVAL: Duration = Duration::from_secs(30);

const CONTENDED_RETRY: Duration = Duration::from_secs(1);
const MAX_CONTENDED_RETRIES: u8 = 3;

pub(super) fn next_background_delay(
    interval: Duration,
    published: bool,
    retries: &mut u8,
) -> Duration {
    if !published && *retries < MAX_CONTENDED_RETRIES {
        *retries += 1;
        CONTENDED_RETRY
    } else {
        *retries = 0;
        interval
    }
}

impl HealthRollupCache {
    pub(super) fn request_health_refresh(&self, age_ms: Option<u64>) {
        if age_ms.is_some_and(|age| age < duration_millis_u64(HEALTH_ROLLUP_TTL)) {
            return;
        }
        self.health_requested.store(true, Ordering::Release);
        if let Some(wake) = self
            .census_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            // A pending bind/census wake also observes health_requested, so a
            // full one-slot channel cannot lose this read's refresh request.
            let _ = wake.try_send(RollupWake::Health);
        }
    }
}

fn advance_age(value: Option<&mut Value>, elapsed: u64) {
    if let Some(value) = value {
        if let Some(age) = value.as_u64() {
            *value = json!(age.saturating_add(elapsed));
        }
    }
}

pub(super) fn advance_memory_ages(metrics: &mut serde_json::Map<String, Value>, elapsed: u64) {
    advance_age(
        metrics
            .get_mut("lsp")
            .and_then(|lsp| lsp.get_mut("process_probe_age_ms")),
        elapsed,
    );
    advance_age(
        metrics
            .get_mut("threads")
            .and_then(|threads| threads.get_mut("classified_age_ms")),
        elapsed,
    );
    if let Some(memory) = metrics.get_mut("memory") {
        advance_age(memory.get_mut("allocator_observation_age_ms"), elapsed);
        if let Some(roots) = memory.get_mut("roots").and_then(Value::as_object_mut) {
            for root in roots.values_mut() {
                advance_age(root.get_mut("memory_measured_age_ms"), elapsed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_rollup_retries_contention_briefly_then_resumes_idle_interval() {
        let delay = |published, retries: &mut u8| {
            next_background_delay(BACKGROUND_INTERVAL, published, retries)
        };
        let retry_delay = Duration::from_secs(1);
        let mut retries = 0;
        assert_eq!(delay(true, &mut retries), BACKGROUND_INTERVAL);
        assert_eq!(delay(false, &mut retries), retry_delay);
        assert_eq!(delay(true, &mut retries), BACKGROUND_INTERVAL);

        let mut elapsed = BACKGROUND_INTERVAL;
        for _ in 0..3 {
            let wait = delay(false, &mut retries);
            assert_eq!(wait, retry_delay);
            elapsed += wait;
        }
        assert!((elapsed.as_millis() as u64) < FINDINGS_SNAPSHOT_MAX_AGE_MS);
        assert_eq!(delay(false, &mut retries), BACKGROUND_INTERVAL);
    }

    fn wait_for_refresh(cache: &HealthRollupCache, count: u64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.refresh_count_for_test() < count {
            assert!(Instant::now() < deadline, "rollup did not publish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn rollup_idle_worker_does_not_recompute_at_read_ttl_but_health_read_wakes_it() {
        let cache = Arc::new(HealthRollupCache::new());
        let worker = HealthRollupWorker::start(
            Arc::clone(&cache),
            Arc::new(Executor::new()),
            crate::test_app(),
        );
        wait_for_refresh(&cache, 1);
        std::thread::sleep(HEALTH_ROLLUP_TTL + Duration::from_millis(100));
        let idle_refreshes = cache.refresh_count_for_test();
        cache.request_health_refresh(cache.snapshot().1);
        wait_for_refresh(&cache, 2);
        worker.shutdown();
        assert_eq!(
            idle_refreshes, 1,
            "an idle worker recomputed at the read TTL"
        );
        assert!(
            cache.allocator.lock().unwrap().is_none(),
            "a health liveness read walked the allocator"
        );
    }

    #[test]
    fn rollup_health_read_requests_refresh_only_after_ttl_without_allocator_work() {
        let cache = HealthRollupCache::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        *cache.census_wake.lock().unwrap() = Some(tx);
        let before = crate::memory::allocator_snapshot_calls_for_test();
        cache.request_health_refresh(Some(2_999));
        assert!(rx.try_recv().is_err());
        cache.request_health_refresh(Some(3_000));
        assert_eq!(rx.try_recv().unwrap(), RollupWake::Health);
        assert!(cache.health_requested.load(Ordering::Acquire));
        assert!(!cache.allocator_requested.load(Ordering::Acquire));
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
    }

    #[test]
    fn rollup_cached_health_read_advances_allocator_and_engine_ages() {
        let mut metrics = json!({"memory": {
            "allocator_observation_age_ms": 300_000,
            "roots": {"root": {"memory_measured_age_ms": 29_000}, "unknown": {}}
        }})
        .as_object()
        .unwrap()
        .clone();
        advance_memory_ages(&mut metrics, 15_000);
        assert_eq!(metrics["memory"]["allocator_observation_age_ms"], 315_000);
        assert_eq!(
            metrics["memory"]["roots"]["root"]["memory_measured_age_ms"],
            44_000
        );
        assert!(metrics["memory"]["roots"]["unknown"]
            .get("memory_measured_age_ms")
            .is_none());
    }
}
