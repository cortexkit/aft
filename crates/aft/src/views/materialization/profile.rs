//! Opt-in wall-time attribution for offline materialization measurements.

use std::time::Instant;

pub(crate) struct PhaseTimer {
    start: Option<Instant>,
    prefix: &'static str,
}

impl PhaseTimer {
    pub(crate) fn new(prefix: &'static str) -> Self {
        Self {
            start: std::env::var_os("AFT_VIEW_PROFILE").map(|_| Instant::now()),
            prefix,
        }
    }

    pub(crate) fn finish(&mut self, phase: &str) {
        if let Some(start) = &mut self.start {
            eprintln!(
                "view_profile {}.{phase} ms={:.3}",
                self.prefix,
                start.elapsed().as_secs_f64() * 1000.0
            );
            *start = Instant::now();
        }
    }
}
