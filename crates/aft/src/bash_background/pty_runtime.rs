use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
pub(crate) struct PtyRuntime {
    pub(crate) master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(crate) killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pub(crate) child_pid: Option<u32>,
    pub(crate) reader_done: Arc<AtomicBool>,
    pub(crate) reader_eof: Arc<AtomicBool>,
    pub(crate) exit_observed: Arc<AtomicBool>,
    pub(crate) was_killed: Arc<AtomicBool>,
    pub(crate) coordinator: Arc<CompletionCoordinator>,
    /// ConPTY needs an explicit close before its output pipe can reach EOF.
    /// Unix runtimes leave this unset; tests can exercise the Windows policy
    /// with a channel-controlled reader on any host.
    #[cfg(any(windows, test))]
    pub(crate) output_drain: Option<PtyOutputDrain>,
}

#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct PtyOutputDrain {
    pub(crate) deadline: Option<std::time::Instant>,
}

pub struct CompletionCoordinator {
    pub task_id: String,
    pub session_id: String,
    pub(crate) remaining: AtomicU8,
    pub(crate) wake_tx: crossbeam_channel::Sender<()>,
    /// Set once the watchdog wake for this task has been sent (both the
    /// reader and the waiter are done). Until then a periodic watchdog pass
    /// leaves an exited task to that wake.
    pub(crate) woken: AtomicBool,
}

impl CompletionCoordinator {
    pub fn new(
        task_id: String,
        session_id: String,
        wake_tx: crossbeam_channel::Sender<()>,
    ) -> Self {
        Self {
            task_id,
            session_id,
            remaining: AtomicU8::new(2),
            wake_tx,
            woken: AtomicBool::new(false),
        }
    }

    pub fn signal_one_done(&self) {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _ = self.wake_tx.try_send(());
            self.woken.store(true, Ordering::SeqCst);
        }
    }
}
