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

/// Only uninterrupted, same-thread intervals have an attributable CPU delta.
/// Queueing, async writes, and aggregate egress span awaits or worker handoffs.
/// Platforms without a sufficiently fine thread clock always report unknown.
#[derive(Debug, Clone, Default)]
pub struct ToolCallPhaseCpu {
    pub setup: Option<Duration>,
    pub translate: Option<Duration>,
    pub execute: Option<Duration>,
    pub format: Option<Duration>,
    pub finalize: Option<Duration>,
    pub egress_prepare: Option<Duration>,
}

/// Causal wait recorded on a tool call so slow-call logs can name the blocker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WaitingOn {
    #[default]
    None,
    Build,
    Limiter,
    ArtifactLoad,
    Resolver,
}

impl WaitingOn {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Build => "build",
            Self::Limiter => "limiter",
            Self::ArtifactLoad => "artifact_load",
            Self::Resolver => "resolver",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCallPhaseDurations {
    pub cpu: ToolCallPhaseCpu,
    pub queue: Duration,
    /// From executor admission to the start of argument translation: the
    /// route's call policy (confinement admission, access-policy scope, the
    /// formatter launch-policy handle) and repeat-breaker capture. Kept apart
    /// from `translate` so per-call policy work is not mistaken for argument
    /// translation.
    pub setup: Duration,
    /// From the start of argument translation to a dispatchable request:
    /// disabled-tool and database gates, hashline registration, and the
    /// translation itself.
    pub translate: Duration,
    pub execute: Duration,
    pub format: Duration,
    pub finalize: Duration,
    pub egress_enqueue: Duration,
    pub egress_queue: Duration,
    pub egress_prepare: Duration,
    pub egress_write: Duration,
    pub egress: Duration,
    pub frame_bytes: usize,
    pub writer_queue_depth: usize,
    pub writer_active_at_enqueue: bool,
    pub writer_queue_was_full: bool,
    pub writer_reserve_timeouts: u32,
    pub total: Duration,
    pub waiting_on: WaitingOn,
    pub waiting_on_build_id: Option<String>,
    pub wait_ms: u64,
    /// Execution includes a requested wait, not just AFT's own work.
    pub intentional_wait: bool,
    /// What the call was asked to do, without any content: the target kind
    /// and, for edits, the match and replacement sizes and the outcome code.
    /// See [`crate::run_tool_call::tool_call_arg_shape`].
    pub arg_shape: Option<String>,
    /// Milliseconds per edit pipeline step, when the call ran any.
    pub edit_phases: Option<String>,
}
