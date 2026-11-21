//! Detailed allocator accounting is demand-driven, never a periodic health probe.

use super::*;

const MIN_REUSE_MS: u64 = 5 * 60 * 1000;

pub(super) struct CachedAllocatorObservation {
    cached_at_ms: u64,
    reuse_ms: u64,
    observation: crate::memory::AllocatorObservation,
}

impl HealthRollupCache {
    pub(super) fn allocator_breakdown_due(&self) -> bool {
        let Ok(cached) = self.allocator.try_lock() else {
            return true;
        };
        cached.as_ref().is_none_or(|cached| {
            self.now_ms().saturating_sub(cached.cached_at_ms) >= cached.reuse_ms
        })
    }

    pub(super) fn allocator_observation(&self) -> crate::memory::AllocatorObservation {
        let requested = self.allocator_requested.swap(false, Ordering::AcqRel);
        let mut cached = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cached.as_ref() {
            let observation = &cached.observation;
            let elapsed_ms = self.now_ms().saturating_sub(cached.cached_at_ms);
            let age_ms = observation.age_ms.unwrap_or(0).saturating_add(elapsed_ms);
            if !requested || elapsed_ms < cached.reuse_ms {
                let mut reused = observation.clone();
                // Unsupported platforms keep a null age, not a measured zero.
                reused.age_ms = observation.age_ms.map(|_| age_ms);
                return reused;
            }
        }
        if !requested {
            // HealthCheck is also a liveness probe. Its RSS/footprint comes
            // from the kernel; allocator fields stay unavailable until an
            // explicit memory.census asks for the expensive breakdown.
            return crate::memory::AllocatorObservation::from_source(
                crate::memory::AllocatorSource::Cached,
            );
        }
        let walk_started = Instant::now();
        let observation = crate::memory::AllocatorObservation::measure();
        let walk_ms = duration_millis_u64(walk_started.elapsed());
        #[cfg(test)]
        let walk_ms = match self.allocator_walk_ms_for_test.load(Ordering::Relaxed) {
            u64::MAX => walk_ms,
            stubbed => stubbed,
        };
        let reuse_ms = crate::memory::allocator_reuse_window_ms(MIN_REUSE_MS, walk_ms);
        // The observation already includes time spent walking the allocator.
        // Reuse starts at completion so the walk is not counted twice.
        *cached = Some(CachedAllocatorObservation {
            cached_at_ms: self.now_ms(),
            reuse_ms,
            observation: observation.clone(),
        });
        observation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_timer_never_walks_allocator_even_after_expiry() {
        let cache = HealthRollupCache::new();
        let before = crate::memory::allocator_snapshot_calls_for_test();
        for now in [0, 3_000, 30_000, 300_001, 3_600_000] {
            cache.clock_ms.store(now, Ordering::Relaxed);
            cache.refresh_for_test(&Executor::new(), &crate::test_app());
        }
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
    }

    #[test]
    fn rollup_explicit_breakdown_is_reused_on_timer_with_its_age() {
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        cache.request_census();
        let first = cache.allocator_observation();
        let before = crate::memory::allocator_snapshot_calls_for_test();
        cache.clock_ms.store(600_000, Ordering::Relaxed);
        let old = cache.allocator_observation();
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
        assert_eq!(old.snapshot, first.snapshot);
        assert_eq!(old.age_ms, first.age_ms.map(|age| age + 600_000));
        cache.request_census();
        cache.allocator_observation();
        assert_eq!(
            crate::memory::allocator_snapshot_calls_for_test(),
            before + 1
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rollup_explicit_census_read_gets_allocator_breakdown_from_worker() {
        let cache = Arc::new(HealthRollupCache::new());
        let worker = HealthRollupWorker::start(
            Arc::clone(&cache),
            Arc::new(Executor::new()),
            crate::test_app(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let census = runtime.block_on(fresh_memory_census(
            &cache,
            Instant::now() + Duration::from_secs(5),
        ));
        worker.shutdown();
        assert_eq!(
            census["process"]["allocator_slack_measured"], true,
            "{census}"
        );
        assert!(census["process"]["allocator_observation_age_ms"].is_number());
        assert!(cache.allocator.lock().unwrap().is_some());
    }
}
