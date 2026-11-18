//! Short-lived process probes for health only. Reaping always probes afresh.

use super::*;

const TTL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
pub(super) struct ProcessProbe {
    pub(super) cwd: ChildCwdState,
    pub(super) rss_bytes: u64,
}

/// Owned by one tracked registration, not by its numeric PID. A re-registration
/// gets a new cache even if the OS immediately reuses the same PID.
#[derive(Debug, Default)]
pub(super) struct HealthProbeCache(Mutex<Option<(Instant, ProcessProbe)>>);

impl HealthProbeCache {
    pub(super) fn read(
        &self,
        now: Instant,
        probe: impl FnOnce() -> ProcessProbe,
    ) -> (ProcessProbe, u64) {
        // A simultaneous health read must not wait on another read's OS call.
        let Ok(mut slot) = self.0.try_lock() else {
            return (probe(), 0);
        };
        if let Some((measured_at, value)) = *slot {
            let age = now.saturating_duration_since(measured_at);
            if age < TTL {
                return (value, u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
            }
        }
        let value = probe();
        *slot = Some((now, value));
        (value, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_pid_probes_reuse_for_three_seconds_and_report_age() {
        let cache = HealthProbeCache::default();
        let now = Instant::now();
        let calls = std::cell::Cell::new(0);
        let probe = || {
            calls.set(calls.get() + 1);
            ProcessProbe {
                cwd: ChildCwdState::Present,
                rss_bytes: calls.get(),
            }
        };
        assert_eq!(cache.read(now, probe).0.rss_bytes, 1);
        let (old, age) = cache.read(now + Duration::from_millis(2_999), probe);
        assert_eq!((old.rss_bytes, age, calls.get()), (1, 2_999, 1));
        let (fresh, age) = cache.read(now + TTL, probe);
        assert_eq!((fresh.rss_bytes, age, calls.get()), (2, 0, 2));
    }

    #[test]
    fn health_pid_reregistration_does_not_reuse_the_previous_process_probe() {
        let registry = LspChildRegistry::new();
        registry.track(std::process::id());
        let first = registry.tracked_children().pop().unwrap().1.health_probe;
        registry.track(std::process::id());
        let second = registry.tracked_children().pop().unwrap().1.health_probe;
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
