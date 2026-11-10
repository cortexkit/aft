//! Wall-clock timing of the stages inside one longer operation, for profiling
//! by hand.

use std::time::Instant;

/// Wall-clock sub-stages of one operation, printed to stderr as
/// `view_profile {prefix}.{stage} ms=...` only when `AFT_VIEW_PROFILE` is set.
/// It records nothing else.
pub(crate) struct StageTimer {
    prefix: &'static str,
    pub(crate) enabled: bool,
    started: Instant,
}

impl StageTimer {
    pub(crate) fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            enabled: std::env::var_os("AFT_VIEW_PROFILE").is_some(),
            started: Instant::now(),
        }
    }

    pub(crate) fn finish(&mut self, stage: &str, detail: std::fmt::Arguments<'_>) {
        #[cfg(test)]
        crate::test_allocations::heap_marker(self.prefix, stage);
        if self.enabled {
            eprintln!(
                "view_profile {}.{stage} ms={:.3} {detail}",
                self.prefix,
                self.started.elapsed().as_secs_f64() * 1000.0
            );
        }
        self.started = Instant::now();
    }
}
