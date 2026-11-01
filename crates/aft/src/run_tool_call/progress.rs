//! A call-scoped, non-blocking progress channel. Only completed work reports here;
//! waiting and cancellation probes are deliberately not progress.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct Progress(Arc<Inner>);

struct Inner {
    last_sent: Mutex<Instant>,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
    send: Box<dyn Fn() -> bool + Send + Sync>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Progress")
    }
}

impl Progress {
    pub(crate) fn new(send: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self::with_clock(Instant::now, send)
    }

    pub(crate) fn with_clock(
        clock: impl Fn() -> Instant + Send + Sync + 'static,
        send: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            last_sent: Mutex::new(clock()),
            clock: Box::new(clock),
            send: Box::new(send),
        }))
    }

    pub(crate) fn report(&self) {
        let now = (self.0.clock)();
        let mut last = self.0.last_sent.lock().unwrap_or_else(|e| e.into_inner());
        if now.saturating_duration_since(*last)
            >= Duration::from_millis(cortexkit_role_tool_provider::KEEPALIVE_INTERVAL_MS)
            && (self.0.send)()
        {
            *last = now;
        }
    }
}

pub(crate) fn report() {
    if let Some(token) = crate::executor::current_job_cancellation() {
        token.report_progress();
    }
}

/// A request may wait for a shared builder on another thread. Observe its
/// completed-work counters, not elapsed time or repeated readiness probes.
pub(crate) struct BuildProgress {
    root: std::path::PathBuf,
    last: Vec<(u64, &'static str, u64, Option<u64>)>,
}

impl BuildProgress {
    pub(crate) fn new(root: &std::path::Path) -> Self {
        Self {
            root: root.to_owned(),
            last: crate::cold_build_limiter::progress::work_for_root(root),
        }
    }

    pub(crate) fn observe(&mut self) {
        let next = crate::cold_build_limiter::progress::work_for_root(&self.root);
        if next != self.last {
            self.last = next;
            report();
        }
    }
}
