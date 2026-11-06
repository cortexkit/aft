//! A call-scoped, non-blocking progress channel. Completed work and live child
//! supervision report here; a blocked observation must not report progress.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct Progress(Arc<Inner>);

struct Inner {
    last_sent: Mutex<Option<Instant>>,
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
            last_sent: Mutex::new(Some(clock())),
            clock: Box::new(clock),
            send: Box::new(send),
        }))
    }

    pub(crate) fn report(&self) {
        let now = (self.0.clock)();
        let mut last = self.0.last_sent.lock().unwrap_or_else(|e| e.into_inner());
        let Some(last) = last.as_mut() else {
            return;
        };
        if now.saturating_duration_since(*last)
            >= Duration::from_millis(cortexkit_role_tool_provider::KEEPALIVE_INTERVAL_MS)
            && (self.0.send)(self.key())
        {
            *last = now;
        }
    }

    /// End the original request's keepalives for every clone, including one
    /// installed by a later approved rerun. Taking the report lock also waits
    /// for any in-flight send before the terminal reply can be queued.
    pub(crate) fn close(&self) {
        self.0
            .last_sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
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

thread_local! {
    static FILES: std::cell::RefCell<Option<FileProgress>> = const { std::cell::RefCell::new(None) };
}

/// Shared scanners outlive individual requests. Count their completed files so
/// only requests waiting for that scanner can turn the counts into keepalive frames.
#[derive(Clone, Default)]
pub(crate) struct FileProgress(Arc<AtomicU64>);

impl FileProgress {
    pub(crate) fn current() -> Option<Self> {
        FILES.with(|files| files.borrow().clone())
    }

    pub(crate) fn enter(&self) -> FileProgressGuard {
        FileProgressGuard {
            previous: FILES.with(|files| files.replace(Some(self.clone()))),
            _thread_bound: std::marker::PhantomData,
        }
    }

    pub(crate) fn finished(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn completed(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

pub(crate) struct FileProgressGuard {
    previous: Option<FileProgress>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for FileProgressGuard {
    fn drop(&mut self) {
        FILES.with(|files| files.replace(self.previous.take()));
    }
}

/// A request may wait for a shared builder on another thread. Observe its
/// completed-work counters, not elapsed time or repeated readiness probes.
pub(crate) struct BuildProgress {
    root: std::path::PathBuf,
    last: Vec<(u64, &'static str, u64, Option<u64>)>,
}

impl BuildProgress {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    #[test]
    fn call_keepalive_close_survives_clone_and_new_job_token() {
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
        let clone = progress.clone();
        clock.store(30_000, Ordering::SeqCst);
        progress.report();
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        progress.close();
        clock.store(60_000, Ordering::SeqCst);
        clone.report();
        let rerun = crate::executor::JobCancellation::new();
        rerun.set_progress(clone);
        clock.store(90_000, Ordering::SeqCst);
        rerun.report_progress();
        assert_eq!(sent.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn call_keepalive_close_waits_for_an_in_flight_send() {
        let clock = Arc::new(AtomicU64::new(0));
        let origin = Instant::now();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let progress = Progress::with_clock(
            {
                let clock = clock.clone();
                move || origin + Duration::from_millis(clock.load(Ordering::SeqCst))
            },
            {
                let entered = entered.clone();
                let release = release.clone();
                move |_| {
                    entered.wait();
                    release.wait();
                    true
                }
            },
        );
        clock.store(30_000, Ordering::SeqCst);
        let reporter = {
            let progress = progress.clone();
            std::thread::spawn(move || progress.report())
        };
        entered.wait();
        let (closing, started) = std::sync::mpsc::channel();
        let (closed, finished) = std::sync::mpsc::channel();
        let closer = std::thread::spawn(move || {
            closing.send(()).unwrap();
            progress.close();
            closed.send(()).unwrap();
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let premature_close = finished.recv_timeout(Duration::from_millis(50));
        release.wait();
        reporter.join().unwrap();
        closer.join().unwrap();
        assert!(matches!(
            premature_close,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
    }
}
