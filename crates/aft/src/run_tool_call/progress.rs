//! A call-scoped, non-blocking progress channel. Only completed work reports here;
//! waiting and cancellation probes are deliberately not progress.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct Progress(Arc<Inner>);

struct Inner {
    last_sent: Mutex<Instant>,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
    send: Box<dyn Fn(usize) -> bool + Send + Sync>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Progress")
    }
}

impl Progress {
    pub(crate) fn new(send: impl Fn(usize) -> bool + Send + Sync + 'static) -> Self {
        Self::with_clock(Instant::now, send)
    }

    pub(crate) fn with_clock(
        clock: impl Fn() -> Instant + Send + Sync + 'static,
        send: impl Fn(usize) -> bool + Send + Sync + 'static,
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
            && (self.0.send)(self.key())
        {
            *last = now;
        }
    }

    pub(crate) fn key(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    #[test]
    fn call_keepalive_shared_build_reports_only_changed_work() {
        let dir = tempfile::tempdir().unwrap();
        let origin = Instant::now();
        let clock = Arc::new(AtomicU64::new(0));
        let sent = Arc::new(AtomicUsize::new(0));
        let progress = Progress::with_clock(
            {
                let clock = clock.clone();
                move || origin + Duration::from_millis(clock.load(Ordering::SeqCst))
            },
            {
                let sent = sent.clone();
                move |_| {
                    sent.fetch_add(1, Ordering::SeqCst);
                    true
                }
            },
        );
        let token = crate::executor::JobCancellation::new();
        token.set_progress(progress);
        let _token = crate::executor::install_job_cancellation(token);
        let mut observer = BuildProgress::new(dir.path());
        let root = dir.path().to_owned();
        let (step, steps) = std::sync::mpsc::channel();
        let (ack, acks) = std::sync::mpsc::channel();
        let builder = std::thread::spawn(move || {
            let _work = crate::cold_build_limiter::progress::start(
                &root,
                "callgraph build",
                Some(2),
                crate::cold_build_limiter::progress::StartLog::Quiet,
            );
            ack.send(()).unwrap();
            while let Ok(advance) = steps.recv() {
                if advance {
                    crate::cold_build_limiter::progress::advance(1);
                }
                ack.send(()).unwrap();
            }
        });
        acks.recv_timeout(Duration::from_secs(5)).unwrap();
        clock.store(30_000, Ordering::SeqCst);
        observer.observe();
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        clock.store(120_000, Ordering::SeqCst);
        step.send(false).unwrap();
        acks.recv_timeout(Duration::from_secs(5)).unwrap();
        observer.observe();
        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "a held builder is not progress"
        );
        step.send(true).unwrap();
        acks.recv_timeout(Duration::from_secs(5)).unwrap();
        observer.observe();
        assert_eq!(
            sent.load(Ordering::SeqCst),
            2,
            "a completed file advances the waiting call"
        );
        drop(step);
        builder.join().unwrap();
    }
}
