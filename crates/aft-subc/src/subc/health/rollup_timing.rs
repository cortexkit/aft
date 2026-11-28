use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub(super) struct RollupStepTimer {
    started: Instant,
    previous: Instant,
    steps: BTreeMap<&'static str, (Duration, u64)>,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    cpu: super::rollup_bench::Timer,
}

impl RollupStepTimer {
    pub(super) fn new() -> Self {
        let started = Instant::now();
        Self {
            started,
            previous: started,
            steps: Default::default(),
            #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
            cpu: super::rollup_bench::Timer::new(),
        }
    }

    pub(super) fn mark(&mut self, step: &'static str) {
        let now = Instant::now();
        let entry = self.steps.entry(step).or_default();
        entry.0 += now.duration_since(self.previous);
        entry.1 += 1;
        self.previous = now;
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        self.cpu.mark(step);
    }

    pub(super) fn metrics(&self) -> Value {
        // Elapsed time includes scheduler waits; the offline benchmark also
        // measures thread CPU so host contention is distinguishable from work.
        let steps = self
            .steps
            .iter()
            .map(|(step, (elapsed, calls))| {
                (
                    (*step).to_owned(),
                    json!({"elapsed_ms": elapsed.as_secs_f64() * 1000.0, "calls": calls}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({"elapsed_ms": self.started.elapsed().as_secs_f64() * 1000.0, "steps": steps})
    }
}

pub(super) fn log_tick(roots: usize, tick: &Value, detail_due: impl FnOnce() -> bool) {
    if log::log_enabled!(log::Level::Debug) {
        // Keep one line per tick without repeating the verbose stage map at
        // the request-driven refresh cadence.
        if detail_due() {
            log::debug!("health rollup tick: roots={roots} {tick}");
        } else {
            log::debug!(
                "health rollup tick: roots={roots} elapsed_ms={}",
                tick["elapsed_ms"]
            );
        }
    }
}
