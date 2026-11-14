//! Best-effort stderr for supervised modules, independent of the file writer.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) const QUEUE_CAPACITY: usize = 256;
const MAX_RECORD_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct MirrorHealth {
    dropped_lines: AtomicU64,
    dropped_flushes: AtomicU64,
    write_errors: AtomicU64,
    flush_errors: AtomicU64,
    start_failures: AtomicU64,
}

impl MirrorHealth {
    pub(super) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "dropped_lines_total": self.dropped_lines.load(Ordering::Relaxed),
            "dropped_flushes_total": self.dropped_flushes.load(Ordering::Relaxed),
            "write_errors_total": self.write_errors.load(Ordering::Relaxed),
            "flush_errors_total": self.flush_errors.load(Ordering::Relaxed),
            "worker_start_failures_total": self.start_failures.load(Ordering::Relaxed),
        })
    }

    fn drop_message(&self, message: &Message) {
        match message {
            Message::Record(bytes) => {
                self.dropped_lines
                    .fetch_add(super::log_line_count(bytes), Ordering::Relaxed);
            }
            Message::Flush | Message::Drain(_) => {
                self.dropped_flushes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

enum Message {
    Record(Vec<u8>),
    Flush,
    Drain(SyncSender<bool>),
}

/// Test-only (debug builds): parks the worker so that records logged during
/// process exit are still queued when the exit fence starts. That is the
/// state a busy host can leave them in when the process exits right after
/// logging them, and the fence must still deliver them.
#[cfg(debug_assertions)]
#[derive(Default)]
struct ExitHold {
    armed: std::sync::Mutex<bool>,
    released: std::sync::Condvar,
}

#[cfg(debug_assertions)]
impl ExitHold {
    fn arm(&self) {
        *self.armed.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    fn release(&self) {
        *self.armed.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.released.notify_all();
    }

    fn wait(&self) {
        let armed = self.armed.lock().unwrap_or_else(|e| e.into_inner());
        // Bounded so a hold that is never released cannot keep a test
        // process's worker parked forever.
        let _ = self
            .released
            .wait_timeout_while(armed, Duration::from_secs(10), |armed| *armed);
    }
}

#[derive(Clone)]
pub(super) struct Mirror {
    tx: Option<SyncSender<Message>>,
    health: Arc<MirrorHealth>,
    #[cfg(debug_assertions)]
    exit_hold: Arc<ExitHold>,
}

impl Mirror {
    pub(super) fn spawn<W: Write + Send + 'static>(
        mut sink: W,
        capacity: usize,
        health: Arc<MirrorHealth>,
    ) -> io::Result<(Self, JoinHandle<()>)> {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let worker_health = Arc::clone(&health);
        #[cfg(debug_assertions)]
        let exit_hold = Arc::new(ExitHold::default());
        #[cfg(debug_assertions)]
        let worker_hold = Arc::clone(&exit_hold);
        let worker = thread::Builder::new()
            .name("aft-stderr-mirror".into())
            .spawn(move || {
                while let Ok(message) = rx.recv() {
                    #[cfg(debug_assertions)]
                    worker_hold.wait();
                    // No channel or file-sink lock is held during stderr I/O.
                    let result = match &message {
                        Message::Record(bytes) => sink.write_all(bytes),
                        Message::Flush | Message::Drain(_) => sink.flush(),
                    };
                    if result.is_err() {
                        match &message {
                            Message::Record(_) => {
                                worker_health.write_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            Message::Flush | Message::Drain(_) => {
                                worker_health.flush_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        worker_health.drop_message(&message);
                    }
                    if let Message::Drain(done) = &message {
                        let _ = done.send(result.is_ok());
                    }
                }
            })?;
        Ok((
            Self {
                tx: Some(tx),
                health,
                #[cfg(debug_assertions)]
                exit_hold,
            },
            worker,
        ))
    }

    pub(super) fn disabled(health: Arc<MirrorHealth>) -> Self {
        health.start_failures.fetch_add(1, Ordering::Relaxed);
        Self {
            tx: None,
            health,
            #[cfg(debug_assertions)]
            exit_hold: Arc::default(),
        }
    }

    pub(super) fn record(&self, bytes: &[u8]) {
        // Reject the entire record before copying, rather than leaving a partial
        // line in the mirror. Both queue length and queued bytes stay bounded.
        if self.tx.is_none() || bytes.len() > MAX_RECORD_BYTES {
            self.health
                .dropped_lines
                .fetch_add(super::log_line_count(bytes), Ordering::Relaxed);
            return;
        }
        self.enqueue(Message::Record(bytes.to_vec()));
    }

    pub(super) fn flush(&self) {
        self.enqueue(Message::Flush);
    }

    // Only process teardown waits for this fence. Ordinary logger flushes stay
    // nonblocking, including when stderr is full or its sink is wedged.
    pub(super) fn drain_on_exit(&self, timeout: Duration) -> bool {
        #[cfg(debug_assertions)]
        self.exit_hold.release();
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        let (done, received) = mpsc::sync_channel(1);
        let mut message = Message::Drain(done);
        loop {
            match tx.try_send(message) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(message)) => {
                    self.health.drop_message(&message);
                    return false;
                }
                Err(TrySendError::Full(returned)) => message = returned,
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.health.drop_message(&message);
                return false;
            }
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        received
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(false)
    }

    /// Test-only (debug builds): stop writing records until `drain_on_exit`
    /// starts. See [`ExitHold`].
    #[cfg(debug_assertions)]
    pub(super) fn hold_until_exit_fence(&self) {
        self.exit_hold.arm();
    }

    fn enqueue(&self, message: Message) {
        let Some(tx) = self.tx.as_ref() else {
            self.health.drop_message(&message);
            return;
        };
        match tx.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(message) | TrySendError::Disconnected(message)) => {
                self.health.drop_message(&message);
            }
        }
    }
}
