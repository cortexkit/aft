//! Stdio writes belong to the server's writer thread, never an executor job.
//! A suspended server can fill its stdin pipe indefinitely. Timing out the
//! acknowledgement leaves the process alive and the frame intact, so resuming
//! the server can finish that frame without corrupting JSON-RPC framing.

use std::io::{self, BufWriter};
use std::process::ChildStdin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};

use super::{transport, LspError};

pub(crate) const STDIN_WRITE_TIMEOUT: Duration = Duration::from_millis(250);

struct WriteJob {
    payload: String,
    done: Sender<io::Result<u64>>,
}

#[derive(Clone)]
pub(crate) struct LspWriter {
    tx: Sender<WriteJob>,
    received: Arc<AtomicU64>,
    order: Arc<AtomicU64>,
    silent_at: Arc<AtomicU64>,
    #[cfg(all(test, unix))]
    raw: Arc<Mutex<BufWriter<ChildStdin>>>,
}

impl LspWriter {
    pub(crate) fn spawn(stdin: ChildStdin) -> io::Result<Self> {
        let (tx, rx) = bounded::<WriteJob>(1);
        let raw = Arc::new(Mutex::new(BufWriter::new(stdin)));
        let worker_raw = Arc::clone(&raw);
        let order = Arc::new(AtomicU64::new(0));
        let worker_order = Arc::clone(&order);
        std::thread::Builder::new()
            .name("aft-lsp-stdin".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let result = worker_raw
                        .lock()
                        .map_err(|_| io::Error::other("writer lock poisoned"))
                        .and_then(|mut writer| {
                            // Order actual FIFO dispatch, not time spent waiting
                            // in the writer queue. Only successful sends expose
                            // this fence to the synchronized document store.
                            let sent_order = worker_order.fetch_add(1, Ordering::SeqCst) + 1;
                            transport::write_message(&mut *writer, &job.payload)
                                .map(|()| sent_order)
                        });
                    let _ = job.done.try_send(result);
                }
            })?;
        Ok(Self {
            tx,
            received: Arc::new(AtomicU64::new(1)),
            order,
            silent_at: Arc::new(AtomicU64::new(0)),
            #[cfg(all(test, unix))]
            raw,
        })
    }

    pub(crate) fn received_count(&self) -> u64 {
        self.received.load(Ordering::Acquire)
    }

    pub(crate) fn note_received(&self) -> u64 {
        self.received.fetch_add(1, Ordering::AcqRel);
        self.order.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) fn mark_unresponsive_if_silent(&self, observed: u64) {
        // Compare epochs rather than clearing a flag: a reply racing this
        // store still makes the server responsive, even if it arrived first.
        if self.received_count() == observed {
            self.silent_at.store(observed, Ordering::Release);
        }
    }

    pub(crate) fn is_unresponsive(&self) -> bool {
        self.silent_at.load(Ordering::Acquire) == self.received_count()
    }

    pub(crate) fn send(&self, payload: String) -> Result<(), LspError> {
        self.send_ordered(payload).map(|_| ())
    }

    pub(crate) fn send_ordered(&self, payload: String) -> Result<u64, LspError> {
        if self.is_unresponsive() {
            return Err(LspError::ServerNotReady("server not responding".into()));
        }
        let observed = self.received_count();
        let deadline = Instant::now() + STDIN_WRITE_TIMEOUT;
        let (done, rx) = bounded(1);
        if self
            .tx
            .send_deadline(WriteJob { payload, done }, deadline)
            .is_err()
        {
            self.mark_unresponsive_if_silent(observed);
            return Err(LspError::ServerNotReady(
                "server not responding: stdin writer unavailable".into(),
            ));
        }
        match rx.recv_deadline(deadline) {
            Ok(result) => result.map_err(Into::into),
            Err(_) => {
                self.mark_unresponsive_if_silent(observed);
                Err(LspError::Timeout(
                    "server not responding: stdin write exceeded 250ms".into(),
                ))
            }
        }
    }

    /// Reader responses and cancellation are best-effort, without a wait that
    /// could strand the reader or extend an already-expired request deadline.
    pub(crate) fn send_best_effort(&self, payload: String) {
        let (done, _) = bounded(1);
        if self.tx.try_send(WriteJob { payload, done }).is_err() {
            self.mark_unresponsive_if_silent(self.received_count());
        }
    }

    /// Give a healthy writer time to drain a burst of server requests, but
    /// never park the stdout reader indefinitely behind a blocked stdin pipe.
    pub(crate) fn send_response(&self, payload: String) -> bool {
        let (done, _) = bounded(1);
        if self
            .tx
            .send_timeout(WriteJob { payload, done }, STDIN_WRITE_TIMEOUT)
            .is_err()
        {
            self.mark_unresponsive_if_silent(self.received_count());
            false
        } else {
            true
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn poison_for_test(&self) {
        let raw = Arc::clone(&self.raw);
        let _ = std::thread::spawn(move || {
            let _guard = raw.lock().expect("writer lock");
            panic!("poison lsp writer for test");
        })
        .join();
    }
}
