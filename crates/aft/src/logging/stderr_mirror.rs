pub use aft_base::logging::stderr_mirror::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::io::{self, Write};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::mpsc::{self, SyncSender, TrySendError};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::thread::{self, JoinHandle};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct GateWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        write_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        flush_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }

    impl Write for GateWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some((entered, release)) = self.write_gate.take() {
                entered.send(()).unwrap();
                let _ = release.recv_timeout(Duration::from_secs(5));
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some((entered, release)) = self.flush_gate.take() {
                entered.send(()).unwrap();
                let _ = release.recv_timeout(Duration::from_secs(5));
            }
            Ok(())
        }
    }

    #[test]
    fn exit_drain_confirms_accepted_records_and_has_a_bounded_wait() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (mirror, worker) = Mirror::spawn(
            GateWriter {
                bytes: bytes.clone(),
                write_gate: Some((entered, released)),
                flush_gate: None,
            },
            8,
            Arc::new(MirrorHealth::default()),
        )
        .unwrap();
        mirror.record(b"shutdown reason\n");
        entering.recv_timeout(Duration::from_secs(1)).unwrap();
        let probe = mirror.clone();
        let (done, received) = mpsc::channel();
        let drainer = thread::spawn(move || {
            let _ = done.send(probe.drain_on_exit(Duration::from_millis(20)));
        });
        let bounded = received.recv_timeout(Duration::from_secs(1));
        // Release the sink before asserting, so even a broken fence cannot leave
        // the worker blocked behind the test's gate after a failure.
        release.send(()).unwrap();
        drainer.join().unwrap();
        assert!(!bounded.expect("exit drain exceeded its wait budget"));
        assert!(mirror.drain_on_exit(Duration::from_secs(1)));
        assert_eq!(&*bytes.lock().unwrap(), b"shutdown reason\n");
        drop(mirror);
        worker.join().unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn exit_hold_keeps_records_queued_until_the_exit_fence_starts() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (mirror, worker) = Mirror::spawn(
            GateWriter {
                bytes: Arc::clone(&bytes),
                write_gate: None,
                flush_gate: None,
            },
            8,
            Arc::new(MirrorHealth::default()),
        )
        .unwrap();
        mirror.hold_until_exit_fence();
        mirror.record(b"process exit marker\n");
        thread::sleep(Duration::from_millis(50));
        let written_while_held = bytes.lock().unwrap().clone();
        let drained = mirror.drain_on_exit(Duration::from_secs(5));
        drop(mirror);
        worker.join().unwrap();
        assert!(
            written_while_held.is_empty(),
            "held worker wrote {:?}",
            String::from_utf8_lossy(&written_while_held)
        );
        assert!(drained, "exit fence did not release the held worker");
        assert_eq!(&*bytes.lock().unwrap(), b"process exit marker\n");
    }

    #[test]
    fn full_queue_drops_whole_records_and_flushes_without_waiting() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let health = Arc::new(MirrorHealth::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mirror, worker) = Mirror::spawn(
            GateWriter {
                bytes: Arc::clone(&bytes),
                write_gate: Some((entered_tx, release_rx)),
                flush_gate: None,
            },
            1,
            Arc::clone(&health),
        )
        .unwrap();
        mirror.record(b"first\n");
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        mirror.record(b"second\n");
        let start = Instant::now();
        mirror.record(b"dropped\n");
        mirror.flush();
        let elapsed = start.elapsed();
        let snapshot = health.snapshot();
        let _ = release_tx.send(());
        drop(mirror);
        worker.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(1),
            "full mirror queue blocked its caller: {elapsed:?}"
        );
        assert_eq!(snapshot["dropped_lines_total"], 1);
        assert_eq!(snapshot["dropped_flushes_total"], 1);
        assert_eq!(&*bytes.lock().unwrap(), b"first\nsecond\n");
    }

    #[test]
    fn blocked_worker_flush_does_not_block_producers_or_health() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let health = Arc::new(MirrorHealth::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mirror, worker) = Mirror::spawn(
            GateWriter {
                bytes: Arc::clone(&bytes),
                write_gate: None,
                flush_gate: Some((entered_tx, release_rx)),
            },
            1,
            Arc::clone(&health),
        )
        .unwrap();
        mirror.flush();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        mirror.record(b"after flush\n");
        mirror.flush();
        let snapshot = health.snapshot();
        let elapsed = start.elapsed();
        let _ = release_tx.send(());
        drop(mirror);
        worker.join().unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert_eq!(snapshot["dropped_flushes_total"], 1);
        assert_eq!(&*bytes.lock().unwrap(), b"after flush\n");
    }

    #[test]
    fn oversized_disconnected_and_unavailable_records_are_counted() {
        let health = Arc::new(MirrorHealth::default());
        let (tx, rx) = mpsc::sync_channel(1);
        let mirror = Mirror {
            tx: Some(tx),
            health: Arc::clone(&health),
            #[cfg(debug_assertions)]
            exit_hold: Arc::default(),
        };
        mirror.record(&vec![b'x'; MAX_RECORD_BYTES + 1]);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(rx);
        mirror.record(b"disconnected\n");
        mirror.flush();
        let disabled = Mirror::disabled(Arc::clone(&health));
        disabled.record(b"disabled\n");
        disabled.flush();
        let snapshot = health.snapshot();
        assert_eq!(snapshot["dropped_lines_total"], 3);
        assert_eq!(snapshot["dropped_flushes_total"], 2);
        assert_eq!(snapshot["worker_start_failures_total"], 1);
    }

    #[test]
    fn sink_errors_are_counted_without_recursive_logging() {
        struct FailedSink;
        impl Write for FailedSink {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let health = Arc::new(MirrorHealth::default());
        let (mirror, worker) = Mirror::spawn(FailedSink, 2, Arc::clone(&health)).unwrap();
        mirror.record(b"one\ntwo\n");
        mirror.flush();
        drop(mirror);
        worker.join().unwrap();
        let snapshot = health.snapshot();
        assert_eq!(snapshot["dropped_lines_total"], 2);
        assert_eq!(snapshot["dropped_flushes_total"], 1);
        assert_eq!(snapshot["write_errors_total"], 1);
        assert_eq!(snapshot["flush_errors_total"], 1);
    }
}
