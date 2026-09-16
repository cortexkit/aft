//! Opt-in investigation timing; no publication policy depends on these samples.

use std::path::{Path, PathBuf};
use std::time::Instant;

pub(super) struct PublicationTiming {
    enabled: bool,
    path: PathBuf,
    started: Instant,
}

impl PublicationTiming {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            enabled: std::env::var_os("AFT_VIEWS_PERF_HUNT").is_some(),
            path: path.to_owned(),
            started: Instant::now(),
        }
    }

    pub(super) fn phase(&mut self, phase: &str) {
        let now = Instant::now();
        if self.enabled {
            log::info!(
                "view_perf_hunt path={} phase={} us={}",
                self.path.display(),
                phase,
                now.duration_since(self.started).as_micros()
            );
        }
        self.started = now;
    }
}
