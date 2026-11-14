pub use aft_base::logging::*;

#[cfg(test)]
mod tests {
    #[test]
    fn watchdog_append_bypasses_held_logger_control_and_follows_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watchdog.log");
        let control = super::FILE_CONTROL.lock().unwrap();
        super::write_watchdog_line(
            &path,
            "stall watchdog: stall detected marker=subc_frame_loop",
        )
        .unwrap();
        let rotated = dir.path().join("watchdog.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        super::write_watchdog_line(&path, "stall watchdog: exit deferred because paging").unwrap();
        super::write_watchdog_line(
            &path,
            "stall watchdog: exiting Authorization: Bearer terminal-secret-value",
        )
        .unwrap();
        drop(control);
        let old = std::fs::read_to_string(rotated).unwrap();
        let current = std::fs::read_to_string(path).unwrap();
        assert!(old.contains("stall detected"));
        assert!(!old.contains("exit deferred"));
        assert_eq!(current.lines().count(), 2);
        assert!(current.contains("exit deferred because paging"));
        assert!(current.contains("stall watchdog: exiting"));
        assert!(!current.contains("terminal-secret-value"));
        assert!(current.contains(crate::log_redact::SECRET_PLACEHOLDER));
    }

    #[test]
    fn gh_shim_preinit_warning_is_durable_and_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("aft-shim.log");
        super::write_preinit_warning_to(&path, "gh speech snapshot invalid: first").unwrap();
        super::write_preinit_warning_to(&path, "gh speech snapshot invalid: second").unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" [aft] WARN gh speech snapshot invalid: first"));
        assert!(lines[1].ends_with(" [aft] WARN gh speech snapshot invalid: second"));
    }

    #[test]
    fn terminal_sync_redacts_both_sinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("terminal.log");
        let sink = super::RotatingFile::unopened(path.clone(), 1024, 1, 1);
        let mut stderr = Vec::new();
        super::write_terminal_line(
            &sink,
            &mut stderr,
            "Authorization: Bearer terminal-secret-value",
        )
        .unwrap();
        let file = std::fs::read(&path).unwrap();
        assert_eq!(file, stderr);
        let line = String::from_utf8(file).unwrap();
        assert!(!line.contains("terminal-secret-value"), "{line}");
        assert!(
            line.contains(crate::log_redact::SECRET_PLACEHOLDER),
            "{line}"
        );
    }

    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    fn isolated_subc_stderr(test_name: &str, body: impl FnOnce()) {
        // libtest's exact names omit the crate prefix from module_path!().
        let test_name = test_name.split_once("::").unwrap().1;
        const PROBE: &str = "AFT_TEST_LOGGING_STDERR_CHILD";
        if std::env::var(PROBE).ok().as_deref() == Some(test_name) {
            super::init_subc();
            body();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--color=never"])
            .env(PROBE, test_name)
            .env("RUST_LOG", "off")
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "stderr child failed: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn hold_stderr() -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            let stderr = io::stderr();
            let _held = stderr.lock();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        (release_tx, holder)
    }

    #[test]
    fn subc_file_writer_warning_does_not_wait_for_stderr() {
        isolated_subc_stderr(
            concat!(
                module_path!(),
                "::subc_file_writer_warning_does_not_wait_for_stderr"
            ),
            || {
                struct ReconfigureFault(Arc<Mutex<Vec<u8>>>);
                impl FileLogSink for ReconfigureFault {
                    fn write_batch(&mut self, lines: &[Vec<u8>]) -> io::Result<()> {
                        for line in lines {
                            self.0.lock().unwrap().extend_from_slice(line);
                        }
                        Ok(())
                    }
                    fn reopen(&mut self) -> io::Result<()> {
                        Ok(())
                    }
                    fn discard_buffer(&mut self) {}
                    fn reconfigure(&mut self, _: &Path) -> io::Result<SweepSummary> {
                        Err(io::Error::other("injected reconfigure failure"))
                    }
                }
                let output = Arc::new(Mutex::new(Vec::new()));
                let sink = Arc::new(Mutex::new(Some(ReconfigureFault(Arc::clone(&output)))));
                let (tx, rx) = mpsc::sync_channel(4);
                let (release, holder) = hold_stderr();
                let worker = thread::spawn(move || {
                    run_file_writer(
                        sink,
                        rx,
                        Arc::new(LogWriteHealth::default()),
                        Duration::ZERO,
                    )
                });
                tx.send(LogMessage::Reconfigure(PathBuf::from("injected-log-root")))
                    .unwrap();
                tx.send(LogMessage::Write(b"after warning\n".to_vec()))
                    .unwrap();
                let (done, completed) = mpsc::sync_channel(1);
                tx.send(LogMessage::Flush(done)).unwrap();
                let progressed = completed.recv_timeout(Duration::from_secs(1));
                let _ = release.send(());
                holder.join().unwrap();
                drop(tx);
                worker.join().unwrap();
                assert_eq!(
                    progressed.ok(),
                    Some(true),
                    "file writer warning waited on stderr"
                );
                assert_eq!(&*output.lock().unwrap(), b"after warning\n");
            },
        );
    }

    #[test]
    fn subc_terminal_mirror_does_not_wait_for_stderr() {
        isolated_subc_stderr(
            concat!(
                module_path!(),
                "::subc_terminal_mirror_does_not_wait_for_stderr"
            ),
            || {
                let directory = tempfile::tempdir().unwrap();
                let tx = FILE_CONTROL.lock().unwrap().tx.clone().unwrap();
                tx.send(LogMessage::Reconfigure(directory.path().to_path_buf()))
                    .unwrap();
                assert!(writer_barrier(&tx));
                let (release, holder) = hold_stderr();
                let (done, completed) = mpsc::channel();
                let caller = thread::spawn(move || {
                    let _ = done.send(write_terminal_log_sync("held-terminal-stderr-probe"));
                });
                // stderr stays held until after this wait, so a write that
                // waited on it would time out here; the budget is generous
                // because a loaded Windows runner can take over a second to
                // persist the line.
                let result = completed.recv_timeout(Duration::from_secs(30));
                let _ = release.send(());
                holder.join().unwrap();
                caller.join().unwrap();
                assert!(
                    matches!(result, Ok(Ok(()))),
                    "terminal stderr blocked persistence: {result:?}"
                );
                let file = directory
                    .path()
                    .join("logs")
                    .join(format!("aft-{}.log", std::process::id()));
                assert!(fs::read_to_string(file)
                    .unwrap()
                    .contains("held-terminal-stderr-probe"));
            },
        );
    }

    #[test]
    fn tee_and_process_diagnostics_cannot_bypass_transport_stderr_policy() {
        let source = include_str!("../../aft-base/src/logging.rs");
        let tee = source
            .split_once("impl Write for TeeWriter {")
            .unwrap()
            .1
            .split_once("fn enqueue_file_line(")
            .unwrap()
            .0;
        assert!(tee.contains("self.stderr.write_all"));
        assert!(tee.contains("self.stderr.flush"));
        assert!(!tee.contains("io::stderr"));
        let diagnostics = source
            .split_once("fn write_stderr_once(")
            .unwrap()
            .1
            .split_once("pub fn flush_durable_log(")
            .unwrap()
            .0;
        assert!(diagnostics.contains("StderrOutput::for_process()"));
        assert!(!diagnostics.contains("io::stderr"));
        let terminal = source
            .split_once("pub fn write_terminal_log_sync(")
            .unwrap()
            .1
            .split_once("fn write_terminal_line(")
            .unwrap()
            .0;
        assert!(terminal.contains("StderrOutput::for_process()"));
        assert!(!terminal.contains("io::stderr"));
    }

    #[test]
    fn blocked_stderr_does_not_stop_redacted_file_logging_or_health() {
        struct Gate {
            entered: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
            bytes: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    let _ = self.release.recv_timeout(Duration::from_secs(5));
                }
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-independent.log");
        let sink = RotatingFile::open(path.clone(), 1024 * 1024, 1, 1).unwrap();
        let (file_tx, file_rx) = mpsc::sync_channel(8);
        let file_worker = thread::spawn(move || {
            run_file_writer(
                Arc::new(Mutex::new(Some(sink))),
                file_rx,
                Arc::new(LogWriteHealth::default()),
                Duration::ZERO,
            )
        });
        let health = Arc::new(stderr_mirror::MirrorHealth::default());
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mirror, stderr_worker) = stderr_mirror::Mirror::spawn(
            Gate {
                entered: Some(entered_tx),
                release: release_rx,
                bytes: Arc::clone(&bytes),
            },
            1,
            Arc::clone(&health),
        )
        .unwrap();
        let mut tee = TeeWriter {
            file_tx: Some(file_tx.clone()),
            stderr: StderrOutput::Subc(mirror),
        };
        tee.write_all(b"Authorization: Bearer mirror-secret-value\n")
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        tee.write_all(b"Authorization: Bearer second-secret-value\n")
            .unwrap();
        tee.write_all(b"third\n").unwrap();
        tee.flush().unwrap();
        let snapshot = health.snapshot();
        let elapsed = start.elapsed();
        let file_progressed = writer_barrier(&file_tx);
        let file = fs::read(&path).unwrap();
        let _ = release_tx.send(());
        drop(tee);
        drop(file_tx);
        stderr_worker.join().unwrap();
        file_worker.join().unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert!(file_progressed, "file writer waited on stderr");
        assert_eq!(snapshot["dropped_lines_total"], 1);
        assert_eq!(snapshot["dropped_flushes_total"], 1);
        let file = String::from_utf8(file).unwrap();
        let mirrored = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        for output in [&file, &mirrored] {
            assert!(!output.contains("mirror-secret-value"), "{output}");
            assert!(!output.contains("second-secret-value"), "{output}");
            assert!(
                output.contains(crate::log_redact::SECRET_PLACEHOLDER),
                "{output}"
            );
        }
        assert!(file.ends_with("third\n"));
        assert!(!mirrored.contains("third"));
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SinkFault {
        None,
        Write,
        Flush,
        Open,
    }

    struct SinkProbe {
        fault: SinkFault,
        output: Vec<u8>,
        opens: usize,
        writes: usize,
        flushes: usize,
    }

    struct FaultWriter {
        probe: Arc<Mutex<SinkProbe>>,
        pending: Vec<u8>,
    }

    impl Write for FaultWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut probe = self.probe.lock().unwrap();
            probe.writes += 1;
            if probe.fault == SinkFault::Write {
                return Err(io::Error::other("No space left on device (injected write)"));
            }
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut probe = self.probe.lock().unwrap();
            probe.flushes += 1;
            if probe.fault == SinkFault::Flush {
                return Err(io::Error::other("No space left on device (injected flush)"));
            }
            probe.output.append(&mut self.pending);
            Ok(())
        }
    }

    struct FaultSink {
        probe: Arc<Mutex<SinkProbe>>,
        writer: Option<BufWriter<FaultWriter>>,
        write_gate: Option<(SyncSender<()>, mpsc::Receiver<()>)>,
    }

    impl FaultSink {
        fn new(fault: SinkFault) -> Self {
            let probe = Arc::new(Mutex::new(SinkProbe {
                fault,
                output: Vec::new(),
                opens: 0,
                writes: 0,
                flushes: 0,
            }));
            Self {
                writer: Some(BufWriter::new(FaultWriter {
                    probe: Arc::clone(&probe),
                    pending: Vec::new(),
                })),
                probe,
                write_gate: None,
            }
        }
    }

    impl FileLogSink for FaultSink {
        fn write_batch(&mut self, lines: &[Vec<u8>]) -> io::Result<()> {
            if let Some((entered, proceed)) = self.write_gate.take() {
                entered.send(()).unwrap();
                proceed.recv().unwrap();
            }
            let writer = self.writer.as_mut().expect("sink must reopen before retry");
            for line in lines {
                writer.write_all(line)?;
            }
            writer.flush()
        }

        fn reopen(&mut self) -> io::Result<()> {
            self.discard_buffer();
            let mut probe = self.probe.lock().unwrap();
            probe.opens += 1;
            if probe.fault == SinkFault::Open {
                return Err(io::Error::other("bad handle (injected reopen)"));
            }
            self.writer = Some(BufWriter::new(FaultWriter {
                probe: Arc::clone(&self.probe),
                pending: Vec::new(),
            }));
            Ok(())
        }

        fn discard_buffer(&mut self) {
            if let Some(writer) = self.writer.take() {
                let _ = writer.into_parts();
            }
        }

        fn reconfigure(&mut self, _root: &Path) -> io::Result<SweepSummary> {
            unreachable!("this test never reconfigures storage")
        }
    }

    fn writer_barrier(tx: &SyncSender<LogMessage>) -> bool {
        let (done, reply) = mpsc::sync_channel(1);
        tx.send(LogMessage::Flush(done))
            .expect("writer still running");
        reply
            .recv_timeout(Duration::from_secs(5))
            .expect("writer must survive a failed log write and answer flush requests")
    }

    #[test]
    fn file_writer_recovers_after_write_and_flush_errors() {
        for fault in [SinkFault::Write, SinkFault::Flush] {
            let sink = FaultSink::new(fault);
            let probe = Arc::clone(&sink.probe);
            let shared = Arc::new(Mutex::new(Some(sink)));
            let health = Arc::new(LogWriteHealth::default());
            let (tx, rx) = mpsc::sync_channel(8);
            let worker_health = Arc::clone(&health);
            let worker = thread::spawn(move || {
                run_file_writer(shared, rx, worker_health, Duration::ZERO);
            });

            tx.send(LogMessage::Write(b"lost one\nlost two\n".to_vec()))
                .unwrap();
            assert!(
                !writer_barrier(&tx),
                "failed writes cannot confirm durability"
            );
            let failed = health.snapshot();
            assert_eq!(failed["dropped_lines_total"], 2);
            assert!(failed["last_write_error"]
                .as_str()
                .unwrap()
                .contains("No space left on device"));
            let since = failed["failing_since"].as_str().unwrap();

            // Opening the replacement handle can fail as well. It must not
            // terminate the worker or erase the original outage's start time.
            probe.lock().unwrap().fault = SinkFault::Open;
            tx.send(LogMessage::Write(b"lost three\n".to_vec()))
                .unwrap();
            assert!(!writer_barrier(&tx));
            assert_eq!(health.snapshot()["dropped_lines_total"], 3);
            assert_eq!(health.snapshot()["failing_since"], since);

            probe.lock().unwrap().fault = SinkFault::None;
            tx.send(LogMessage::Write(b"recovered\n".to_vec())).unwrap();
            assert!(writer_barrier(&tx));
            tx.send(LogMessage::Write(b"still logging\n".to_vec()))
                .unwrap();
            assert!(writer_barrier(&tx));
            drop(tx);
            worker.join().unwrap();

            let probe = probe.lock().unwrap();
            let output = String::from_utf8(probe.output.clone()).unwrap();
            let lines: Vec<_> = output.lines().collect();
            assert_eq!(lines.len(), 3, "no replay of lost buffers: {output}");
            assert!(
                lines[0].contains(&format!("log writes failed from {since} to ")),
                "{output}"
            );
            assert!(lines[0].contains("3 lines dropped"), "{output}");
            assert!(
                lines[0].contains("bad handle (injected reopen)"),
                "{output}"
            );
            assert_eq!(lines[1], "recovered");
            assert_eq!(lines[2], "still logging");
            assert_eq!(probe.opens, 2);
            let recovered = health.snapshot();
            assert_eq!(recovered["dropped_lines_total"], 3);
            assert!(recovered["failing_since"].is_null());
            assert_eq!(
                recovered["last_write_error"],
                "bad handle (injected reopen)"
            );
        }
    }

    #[test]
    fn log_write_retries_back_off_without_sleeping_or_replaying_lost_lines() {
        let mut sink = FaultSink::new(SinkFault::Write);
        let health = Arc::new(LogWriteHealth::default());
        let mut recovery = LogWriteRecovery::new(Arc::clone(&health), LOG_WRITE_RETRY_MIN);
        let now = Instant::now();
        assert!(!recovery.write_batch(&mut sink, &line("lost one"), now));
        let writes = sink.probe.lock().unwrap().writes;
        assert!(!recovery.write_batch(
            &mut sink,
            &line("lost two"),
            now + Duration::from_millis(999)
        ));
        assert_eq!(sink.probe.lock().unwrap().writes, writes);
        assert_eq!(sink.probe.lock().unwrap().opens, 0);

        sink.probe.lock().unwrap().fault = SinkFault::Open;
        assert!(!recovery.write_batch(
            &mut sink,
            &line("lost three"),
            now + Duration::from_secs(1)
        ));
        assert_eq!(sink.probe.lock().unwrap().opens, 1);
        assert!(!recovery.write_batch(
            &mut sink,
            &line("lost four"),
            now + Duration::from_millis(2999)
        ));
        assert_eq!(
            sink.probe.lock().unwrap().opens,
            1,
            "reopen failures also back off"
        );

        sink.probe.lock().unwrap().fault = SinkFault::None;
        assert!(recovery.write_batch(&mut sink, &line("recovered"), now + Duration::from_secs(3)));
        let output = String::from_utf8(sink.probe.lock().unwrap().output.clone()).unwrap();
        assert!(
            output.lines().next().unwrap().contains("4 lines dropped"),
            "{output}"
        );
        assert!(output.ends_with("\nrecovered\n"), "{output}");
        assert_eq!(health.snapshot()["dropped_lines_total"], 4);
        assert_eq!(recovery.retry_delay, LOG_WRITE_RETRY_MIN);

        // Repeated failures must reach a ceiling, not a busy retry loop or an
        // unbounded delay that effectively disables the log again.
        sink.probe.lock().unwrap().fault = SinkFault::Write;
        let mut next = now + Duration::from_secs(4);
        for _ in 0..10 {
            assert!(!recovery.write_batch(&mut sink, &line("lost again"), next));
            next = recovery.outage.as_ref().unwrap().retry_at;
        }
        assert_eq!(recovery.retry_delay, LOG_WRITE_RETRY_MAX);
        assert_eq!(
            next.duration_since(now + Duration::from_secs(4)),
            Duration::from_secs(181)
        );
    }

    #[test]
    fn log_callers_and_health_do_not_wait_for_a_stalled_sink_or_full_queue() {
        let mut sink = FaultSink::new(SinkFault::Write);
        let (entered, entering) = mpsc::sync_channel(1);
        let (proceed, proceeding) = mpsc::sync_channel(1);
        sink.write_gate = Some((entered, proceeding));
        let shared = Arc::new(Mutex::new(Some(sink)));
        let health = Arc::new(LogWriteHealth::default());
        let (tx, rx) = mpsc::sync_channel(1);
        let worker_health = Arc::clone(&health);
        let worker =
            thread::spawn(move || run_file_writer(shared, rx, worker_health, Duration::ZERO));
        tx.send(LogMessage::Write(b"in flight\n".to_vec())).unwrap();
        entering
            .recv_timeout(Duration::from_secs(5))
            .expect("worker entered sink I/O");

        let caller_health = Arc::clone(&health);
        let caller_tx = tx.clone();
        let (finished, finishing) = mpsc::sync_channel(1);
        let caller = thread::spawn(move || {
            let mut tx = Some(caller_tx);
            enqueue_file_line(&mut tx, b"queued\n", &caller_health);
            enqueue_file_line(&mut tx, b"queue full one\n", &caller_health);
            enqueue_file_line(&mut tx, b"queue full two\n", &caller_health);
            finished.send(caller_health.snapshot()).unwrap();
        });
        let while_stalled = finishing
            .recv_timeout(Duration::from_secs(5))
            .expect("log caller and health must finish while sink I/O is still stalled");
        assert_eq!(while_stalled["dropped_lines_total"], 2);
        proceed.send(()).unwrap();
        caller.join().unwrap();
        assert!(!writer_barrier(&tx));
        drop(tx);
        worker.join().unwrap();
        assert_eq!(health.snapshot()["dropped_lines_total"], 4);
    }

    #[test]
    // Unix only: the fault is staged by renaming the log directory while the
    // sink holds a file open in it, which Windows refuses (Access is denied).
    // The injected-sink tests cover the same recovery path on every platform.
    #[cfg(unix)]
    fn file_log_reopens_after_rotation_loses_its_handle() {
        let temp = TempDir::new().unwrap();
        let logs = temp.path().join("logs");
        create_private_log_dir(&logs).unwrap();
        let path = logs.join("aft-777.log");
        let mut sink = RotatingFile::open(path.clone(), 100, 1, 1).unwrap();
        sink.write_batch(&line("before outage")).unwrap();
        let health = Arc::new(LogWriteHealth::default());
        let mut recovery = LogWriteRecovery::new(Arc::clone(&health), Duration::ZERO);

        // A regular file at the directory path makes rotation fail after it
        // takes the active handle, with no reliance on disk usage or modes.
        let moved = temp.path().join("moved-logs");
        fs::rename(&logs, &moved).unwrap();
        fs::write(&logs, b"not a directory").unwrap();
        assert!(!recovery.write_batch(&mut sink, &line(&"x".repeat(101)), Instant::now()));
        assert!(sink.writer.is_none());
        fs::remove_file(&logs).unwrap();
        fs::rename(&moved, &logs).unwrap();
        // Do not rotate again: examine the original file's append ordering.
        sink.threshold = LOG_FILE_BYTES;
        assert!(recovery.write_batch(&mut sink, &line("after outage"), Instant::now()));
        let output = fs::read_to_string(path).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 3, "{output}");
        assert_eq!(lines[0], "before outage");
        assert!(lines[1].contains("1 lines dropped"), "{output}");
        assert_eq!(lines[2], "after outage");
        assert_eq!(health.snapshot()["dropped_lines_total"], 1);
    }

    #[test]
    fn file_log_recovers_when_initial_open_failed() {
        let temp = TempDir::new().unwrap();
        let logs = temp.path().join("logs");
        fs::write(&logs, b"not a directory").unwrap();
        let path = logs.join("aft-777.log");
        let health = Arc::new(LogWriteHealth::default());
        let (mut sink, summary) = initial_file_sink(&logs, &path, Arc::clone(&health));
        assert!(summary.is_none());
        assert!(sink.writer.is_none());
        assert!(health.snapshot()["last_write_error"].is_string());
        let mut recovery = LogWriteRecovery::new(Arc::clone(&health), Duration::ZERO);
        assert!(!recovery.write_batch(&mut sink, &line("lost startup line"), Instant::now()));
        fs::remove_file(&logs).unwrap();
        assert!(recovery.write_batch(&mut sink, &line("startup recovered"), Instant::now()));
        let output = fs::read_to_string(path).unwrap();
        assert!(
            output.lines().next().unwrap().contains("1 lines dropped"),
            "{output}"
        );
        assert!(output.ends_with("\nstartup recovered\n"), "{output}");
        assert!(health.snapshot()["failing_since"].is_null());
    }

    #[test]
    fn panic_hook_child_probe() {
        if std::env::var_os("AFT_TEST_PANIC_LOG_CHILD").is_none() {
            return;
        }
        let path = crate::storage_path::storage_dir(None)
            .join("logs")
            .join(format!("aft-{}.log", std::process::id()));
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let persisted =
                fs::read_to_string(&path).is_ok_and(|text| text.contains("forced backtrace:"));
            eprintln!("panic diagnostic persisted before previous hook: {persisted}");
            previous(info);
        }));
        init();
        let backtrace_env = std::env::var_os("RUST_BACKTRACE");
        let panic = std::panic::catch_unwind(|| panic!("durable panic diagnostic probe"));
        assert!(panic.is_err());
        assert_eq!(std::env::var_os("RUST_BACKTRACE"), backtrace_env);
        // No writer flush or destructor: exercise persistence before an abrupt
        // daemon exit, not merely a diagnostic still queued in memory.
        std::process::exit(0);
    }

    #[test]
    fn panic_hook_preserves_default_backtrace_behavior_and_persists() {
        for backtrace in [None, Some("0"), Some("1"), Some("full")] {
            let storage = TempDir::new().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "logging::tests::panic_hook_child_probe",
                    "--nocapture",
                ])
                .env("AFT_TEST_PANIC_LOG_CHILD", "1")
                .env("AFT_STORAGE_DIR", storage.path())
                .env("RUST_LOG", "off")
                .env_remove("RUST_BACKTRACE");
            if let Some(value) = backtrace {
                child.env("RUST_BACKTRACE", value);
            }
            let output = child.output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "child failed: {stderr}");
            let paths = fs::read_dir(storage.path().join("logs")).unwrap();
            let log = paths
                .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect::<String>();
            assert!(log.contains("durable panic diagnostic probe"), "{log}");
            assert!(log.contains("forced backtrace:"), "{log}");
            assert!(log.contains("panic_hook_child_probe"), "{log}");
            assert!(stderr.contains("panic diagnostic persisted before previous hook: true"));
            assert!(stderr.contains("durable panic diagnostic probe"));
            assert_eq!(
                stderr.contains("stack backtrace:"),
                matches!(backtrace, Some("1" | "full")),
                "RUST_BACKTRACE={backtrace:?}: {stderr}"
            );
            if backtrace == Some("full") {
                assert!(
                    stderr.contains("0x"),
                    "default full backtrace was lost: {stderr}"
                );
            }
        }
    }

    #[test]
    fn panic_diagnostic_text_is_bounded_at_utf8_boundaries() {
        let enormous = "é🚀".repeat(PANIC_BACKTRACE_BYTES);
        for budget in [PANIC_MESSAGE_BYTES, PANIC_BACKTRACE_BYTES] {
            let text = bounded_panic_text(format_args!("{enormous}"), budget);
            assert!(text.len() <= budget);
            assert!(text.ends_with(PANIC_TRUNCATION));
            assert!(text.starts_with("é🚀"));
        }
        assert_eq!(
            bounded_panic_text(format_args!("small"), PANIC_MESSAGE_BYTES),
            "small"
        );
    }

    fn line(value: &str) -> Vec<Vec<u8>> {
        vec![format!("{value}\n").into_bytes()]
    }

    fn render_log_line(level: log::Level, target: &str, message: &str) -> String {
        let mut out = Vec::new();
        write_log_line(
            &mut out,
            "2026-09-26T10:00:00Z",
            level,
            target,
            &format_args!("{message}"),
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn log_lines_carry_their_level_after_the_tag() {
        assert_eq!(
            render_log_line(log::Level::Warn, "aft::subc", "slow tool_call name=read"),
            "2026-09-26T10:00:00Z [aft] WARN slow tool_call name=read\n"
        );
        assert_eq!(
            render_log_line(log::Level::Error, "aft::lsp::client", "server crashed"),
            "2026-09-26T10:00:00Z [aft-lsp] ERROR server crashed\n"
        );
        assert_eq!(
            render_log_line(log::Level::Info, "aft", "started"),
            "2026-09-26T10:00:00Z [aft] INFO started\n"
        );
        assert_eq!(
            render_log_line(log::Level::Debug, "aft_lsp", "x"),
            "2026-09-26T10:00:00Z [aft-lsp] DEBUG x\n"
        );
    }

    fn slow_phases() -> ToolCallPhaseDurations {
        ToolCallPhaseDurations {
            cpu: crate::tool_progress::ToolCallPhaseCpu::default(),
            queue: Duration::ZERO,
            setup: Duration::ZERO,
            translate: Duration::ZERO,
            execute: Duration::from_millis(120),
            format: Duration::ZERO,
            finalize: Duration::ZERO,
            egress_enqueue: Duration::ZERO,
            egress_queue: Duration::ZERO,
            egress_prepare: Duration::ZERO,
            egress_write: Duration::ZERO,
            egress: Duration::ZERO,
            frame_bytes: 10,
            writer_queue_depth: 0,
            writer_active_at_enqueue: false,
            writer_queue_was_full: false,
            writer_reserve_timeouts: 0,
            total: SLOW_TOOL_CALL_THRESHOLD + Duration::from_millis(70),
            waiting_on: WaitingOn::None,
            waiting_on_build_id: None,
            wait_ms: 0,
            intentional_wait: false,
            arg_shape: None,
            edit_phases: None,
        }
    }

    fn slow_call_levels(name: &str, caller: ToolCallCaller) -> Vec<log::Level> {
        SLOW_TOOL_CALL_CAPTURE.with(|captured| captured.borrow_mut().clear());
        note_tool_call_trace(
            name,
            "test-session",
            Path::new("/repo"),
            7,
            1,
            slow_phases(),
            caller,
        );
        SLOW_TOOL_CALL_CAPTURE.with(|captured| {
            captured
                .borrow_mut()
                .drain(..)
                .inspect(|(_, message)| {
                    assert!(message.starts_with(&format!("slow tool_call name={name} ")))
                })
                .map(|(level, _)| level)
                .collect()
        })
    }

    #[test]
    fn slow_phase_time_renders_wall_and_cpu_in_one_field() {
        assert_eq!(
            slow_phase_time(
                Duration::from_micros(39_123),
                Some(Duration::from_micros(4))
            ),
            "39.123ms/cpu=0.004ms"
        );
        assert_eq!(
            slow_phase_time(Duration::from_millis(50), None),
            "50.000ms/cpu=?"
        );
    }

    #[test]
    fn slow_plumbing_call_logs_at_debug_while_slow_agent_call_warns() {
        assert_eq!(
            slow_call_levels("bash_drain_completions", ToolCallCaller::Plumbing),
            vec![log::Level::Debug]
        );
        assert_eq!(
            slow_call_levels("bash_status", ToolCallCaller::Plumbing),
            vec![log::Level::Debug]
        );
        assert_eq!(
            slow_call_levels("bash_status", ToolCallCaller::Agent),
            vec![log::Level::Warn]
        );
        assert_eq!(
            slow_call_levels("read", ToolCallCaller::Agent),
            vec![log::Level::Warn]
        );
    }

    #[test]
    fn slow_intentional_wait_excludes_execution_but_warns_for_overhead() {
        let mut phases = slow_phases();
        phases.intentional_wait = true;
        phases.execute = Duration::from_secs(120);
        phases.total = phases.execute + SLOW_TOOL_CALL_THRESHOLD;
        let _ = take_slow_tool_call_logs_for_test();
        note_tool_call_trace(
            "bash_watch",
            "waiting-session",
            Path::new("/repo"),
            7,
            1,
            phases.clone(),
            ToolCallCaller::Agent,
        );
        assert!(take_slow_tool_call_logs_for_test().is_empty());

        phases.queue = SLOW_TOOL_CALL_THRESHOLD + Duration::from_millis(1);
        phases.total = phases.execute + phases.queue;
        note_tool_call_trace(
            "bash_watch",
            "waiting-session",
            Path::new("/repo"),
            7,
            2,
            phases.clone(),
            ToolCallCaller::Agent,
        );
        let lines = SLOW_TOOL_CALL_CAPTURE.with(|captured| captured.take());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, log::Level::Warn);

        phases.intentional_wait = false;
        phases.queue = Duration::ZERO;
        phases.total = phases.execute;
        note_tool_call_trace(
            "bash",
            "waiting-session",
            Path::new("/repo"),
            7,
            3,
            phases,
            ToolCallCaller::Agent,
        );
        let lines = SLOW_TOOL_CALL_CAPTURE.with(|captured| captured.take());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, log::Level::Warn);
    }

    #[test]
    fn fast_call_logs_no_slow_line() {
        let mut phases = slow_phases();
        phases.total = SLOW_TOOL_CALL_THRESHOLD;
        SLOW_TOOL_CALL_CAPTURE.with(|captured| captured.borrow_mut().clear());
        note_tool_call_trace(
            "read",
            "test-session",
            Path::new("/repo"),
            7,
            1,
            phases,
            ToolCallCaller::Agent,
        );
        assert!(SLOW_TOOL_CALL_CAPTURE.with(|captured| captured.borrow().is_empty()));
    }

    #[test]
    fn subc_tool_catalog_fast_answer_does_not_emit_slow_call_line() {
        let mut phases = slow_phases();
        phases.total = SLOW_TOOL_CALL_THRESHOLD;
        let _ = take_slow_tool_call_logs_for_test();
        note_tool_call_trace(
            "tool.catalog",
            "catalog-fast",
            Path::new("/repo"),
            7,
            1,
            phases,
            ToolCallCaller::Agent,
        );
        assert!(take_slow_tool_call_logs_for_test().is_empty());
    }

    #[test]
    fn epoch_timestamp_renders_known_dates() {
        // Epoch start, a modern date, a post-2038 date (u64 range), and the
        // 2100 non-leap century boundary that naive leap logic gets wrong.
        assert_eq!(format_epoch_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(1_704_067_200), "2024-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(format_epoch_secs(4_102_444_800), "2100-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(4_107_542_399), "2100-02-28T23:59:59Z");
    }

    #[test]
    fn token_written_through_the_sink_is_masked_on_disk() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("aft-777.log");
        let sink = RotatingFile::open(path.clone(), LOG_FILE_BYTES, 1, 1).unwrap();
        let (tx, rx) = mpsc::sync_channel(8);
        let sink = Arc::new(Mutex::new(Some(sink)));
        let writer = thread::spawn(move || {
            run_file_writer(
                sink,
                rx,
                Arc::new(LogWriteHealth::default()),
                LOG_WRITE_RETRY_MIN,
            )
        });
        let mut tee = TeeWriter::new(Some(tx));
        let token = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        tee.write_all(
            format!("[aft] fetch https://bob:pw123@host.example/x?access_token=zz9 {token}\n")
                .as_bytes(),
        )
        .unwrap();
        drop(tee);
        writer.join().unwrap();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk,
            "[aft] fetch https://***@host.example/x?access_token=<REDACTED_SECRET> <REDACTED_SECRET>\n"
        );
        assert!(!on_disk.contains(token));
        assert!(!on_disk.contains("pw123"));
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn fresh_log_dir_and_file_are_owner_only() {
        let temp = TempDir::new().unwrap();
        let logs_dir = temp.path().join("storage").join("logs");
        let path = logs_dir.join("aft-4242.log");
        let (mut sink, _) = prepare_file_sink(&logs_dir, &path).unwrap();
        sink.write_batch(&line("hello")).unwrap();

        assert_eq!(mode_of(&logs_dir), 0o700);
        assert_eq!(mode_of(&path), 0o600);
        // A rotated-in generation is created owner-only too.
        sink.rotate().unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(&rotated_path(&path, 1)), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn existing_world_readable_log_dir_and_files_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let logs_dir = temp.path().join("logs");
        fs::create_dir(&logs_dir).unwrap();
        fs::set_permissions(&logs_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = logs_dir.join("aft-4242.log");
        for file in [path.clone(), rotated_path(&path, 1)] {
            fs::write(&file, "old\n").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let _ = prepare_file_sink(&logs_dir, &path).unwrap();

        assert_eq!(mode_of(&logs_dir), 0o700);
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(&rotated_path(&path, 1)), 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
    }

    #[test]
    fn rotation_rolls_once_and_replaces_the_single_backup_generation() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("aft-123.log");
        fs::write(rotated_path(&path, 1), "stale backup\n").unwrap();
        let mut sink = RotatingFile::open(path.clone(), 10, 1, 1).unwrap();
        sink.write_batch(&line("aaaa")).unwrap();
        sink.write_batch(&line("bbbb")).unwrap();
        sink.write_batch(&line("cccc")).unwrap();
        sink.write_batch(&line("dddd")).unwrap();
        sink.write_batch(&line("eeee")).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "eeee\n");
        assert_eq!(
            fs::read_to_string(rotated_path(&path, 1)).unwrap(),
            "cccc\ndddd\n"
        );
        assert!(!rotated_path(&path, 2).exists());
    }

    #[test]
    fn dead_pid_sweep_respects_age_liveness_and_explicit_plugin_exclusion() {
        let temp = TempDir::new().unwrap();
        let dead = temp.path().join("aft-4294967294.log");
        let dead_rotated = temp.path().join("aft-4294967294.log.1");
        let fresh_dead = temp.path().join("aft-4294967293.log");
        let own = temp.path().join(format!("aft-{}.log", std::process::id()));
        let live_rotated = rotated_path(&own, 1);
        let plugin = temp.path().join("aft-plugin.log");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        for path in [&dead, &dead_rotated, &own, &live_rotated, &plugin] {
            fs::write(path, "log").unwrap();
            set_file_mtime(path, FileTime::from_unix_time(1, 0)).unwrap();
        }
        fs::write(&fresh_dead, "fresh").unwrap();
        set_file_mtime(
            &fresh_dead,
            FileTime::from_unix_time(
                (now - DEAD_PROCESS_LOG_MAX_AGE + Duration::from_secs(1))
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
                0,
            ),
        )
        .unwrap();

        let summary = sweep_logs(temp.path(), now, DEAD_PROCESS_LOG_MAX_AGE, u64::MAX).unwrap();

        assert_eq!(summary.removed_files, 2);
        assert!(!dead.exists());
        assert!(!dead_rotated.exists());
        assert!(fresh_dead.exists());
        assert!(own.exists());
        assert!(live_rotated.exists());
        assert!(plugin.exists());
    }

    /// A live process's own start time always precedes its writes, so its file
    /// with a current mtime stays; the same live number on a file last written
    /// before that process started is a recycled PID and is reaped.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn recycled_pid_log_is_reaped_while_the_live_owner_is_kept() {
        let temp = TempDir::new().unwrap();
        let live_pid = unsafe { libc::getppid() } as u32;
        assert!(is_process_alive(live_pid));
        let recycled = temp.path().join(format!("aft-{live_pid}.log"));
        fs::write(&recycled, "written long before this process existed").unwrap();
        set_file_mtime(&recycled, FileTime::from_unix_time(1, 0)).unwrap();

        let summary = sweep_logs(
            temp.path(),
            SystemTime::now(),
            DEAD_PROCESS_LOG_MAX_AGE,
            u64::MAX,
        )
        .unwrap();
        assert_eq!(summary.removed_files, 1);
        assert_eq!(summary.recycled_pids, 1);
        assert!(!recycled.exists());

        let owned = temp.path().join(format!("aft-{live_pid}.log"));
        fs::write(&owned, "written by the live owner").unwrap();
        let summary = sweep_logs(
            temp.path(),
            SystemTime::now() + DEAD_PROCESS_LOG_MAX_AGE * 2,
            DEAD_PROCESS_LOG_MAX_AGE,
            0,
        )
        .unwrap();
        assert_eq!(summary.removed_files, 0);
        assert!(owned.exists());
    }

    /// After a reboot the kernel hands low numbers to system daemons owned by
    /// other users. PID 1 is always such a process, and the first cut of the
    /// recycled-PID check could not read its start time on macOS, so the file
    /// stayed pinned as if the daemon had written it. The verdict must not
    /// depend on the new owner's uid.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn recycled_pid_now_owned_by_another_user_is_reaped() {
        let temp = TempDir::new().unwrap();
        assert!(is_process_alive(1));
        assert!(
            crate::process_identity::process_start_time_ms(1).is_some(),
            "pid 1's start time must be readable from an unprivileged process"
        );
        let recycled = temp.path().join("aft-1.log");
        fs::write(&recycled, "written before the current pid 1 booted").unwrap();
        set_file_mtime(&recycled, FileTime::from_unix_time(1, 0)).unwrap();

        let summary = sweep_logs(
            temp.path(),
            SystemTime::now(),
            DEAD_PROCESS_LOG_MAX_AGE,
            u64::MAX,
        )
        .unwrap();
        assert_eq!(summary.removed_files, 1);
        assert_eq!(summary.recycled_pids, 1);
        assert!(!recycled.exists());
    }

    #[test]
    fn budget_backstop_deletes_oldest_dead_files_but_not_live_files() {
        let temp = TempDir::new().unwrap();
        let oldest = temp.path().join("aft-4294967294.log");
        let newest = temp.path().join("aft-4294967293.log");
        let live = temp.path().join(format!("aft-{}.log", std::process::id()));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        fs::write(&oldest, "oldest").unwrap();
        fs::write(&newest, "newest").unwrap();
        fs::write(&live, "live-live").unwrap();
        set_file_mtime(&oldest, FileTime::from_unix_time(1, 0)).unwrap();
        set_file_mtime(&newest, FileTime::from_unix_time(2, 0)).unwrap();
        set_file_mtime(&live, FileTime::from_unix_time(1, 0)).unwrap();

        let summary = sweep_logs(
            temp.path(),
            now,
            Duration::from_secs(365 * 24 * 60 * 60),
            15,
        )
        .unwrap();

        assert_eq!(summary.removed_files, 1);
        assert!(!oldest.exists());
        assert!(newest.exists());
        assert!(live.exists());
    }

    #[test]
    fn budget_backstop_keeps_the_newest_dead_log_inside_its_forensic_window() {
        // 2026-09-18: a successor's startup sweep deleted the log of the
        // process that had just died, because the directory was over budget
        // and that log was the only eligible candidate. The newest dead log
        // younger than the age reap is the record of the last exit and stays.
        let temp = TempDir::new().unwrap();
        let older = temp.path().join("aft-4294967294.log");
        let predecessor = temp.path().join("aft-4294967293.log");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        fs::write(&older, "older-older-older").unwrap();
        fs::write(&predecessor, "predecessor-predecessor").unwrap();
        set_file_mtime(&older, FileTime::from_unix_time(1, 0)).unwrap();
        // Modified moments before `now`: dead, but inside the 24 h window.
        set_file_mtime(
            &predecessor,
            FileTime::from_unix_time(10 * 24 * 60 * 60 - 30, 0),
        )
        .unwrap();

        let summary = sweep_logs(temp.path(), now, DEAD_PROCESS_LOG_MAX_AGE, 4).unwrap();

        assert!(
            !older.exists(),
            "the older dead log is still budget-reapable"
        );
        assert!(predecessor.exists(), "the newest dead log is protected");
        assert_eq!(summary.removed_files, 1);
        assert_eq!(
            summary.over_budget_bytes,
            "predecessor-predecessor".len() as u64 - 4,
            "the shortfall is reported instead of destroying the record"
        );

        // Once it is older than the age reap it is an ordinary dead log again.
        set_file_mtime(&predecessor, FileTime::from_unix_time(1, 0)).unwrap();
        let summary = sweep_logs(temp.path(), now, DEAD_PROCESS_LOG_MAX_AGE, 4).unwrap();
        assert!(!predecessor.exists());
        assert_eq!(summary.over_budget_bytes, 0);
    }

    #[test]
    fn relic_plugin_rotations_are_reaped_before_process_logs_are_weighed() {
        let temp = TempDir::new().unwrap();
        let active = temp.path().join("aft-plugin.log");
        let backup = temp.path().join("aft-plugin.log.1");
        let relic_two = temp.path().join("aft-plugin.log.2");
        let relic_five = temp.path().join("aft-plugin.log.5");
        let test_relic = temp.path().join("aft-plugin-test.log.3");
        let predecessor = temp.path().join("aft-4294967293.log");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        for (path, content) in [
            (&active, "active"),
            (&backup, "backup"),
            (&relic_two, "relic-two-relic-two"),
            (&relic_five, "relic-five-relic-five"),
            (&test_relic, "test-relic-test-relic"),
            (&predecessor, "predecessor"),
        ] {
            fs::write(path, content).unwrap();
            set_file_mtime(path, FileTime::from_unix_time(10 * 24 * 60 * 60 - 30, 0)).unwrap();
        }

        // Budget fits the owned files plus the predecessor only once the
        // relics are gone; the predecessor must not be the thing that pays.
        let budget = ("active".len() + "backup".len() + "predecessor".len()) as u64;
        let summary = sweep_logs(temp.path(), now, DEAD_PROCESS_LOG_MAX_AGE, budget).unwrap();

        assert!(!relic_two.exists());
        assert!(!relic_five.exists());
        assert!(!test_relic.exists());
        assert!(active.exists(), "the plugin's live file is never touched");
        assert!(
            backup.exists(),
            "the plugin's one owned backup is never touched"
        );
        assert!(predecessor.exists());
        assert_eq!(summary.removed_files, 3);
        assert_eq!(summary.over_budget_bytes, 0);
    }

    #[test]
    fn tool_call_summary_uses_bounded_window_median_and_maxima() {
        let samples = VecDeque::from([
            ToolCallPerfSample {
                total_ms: 9,
                queue_ms: 5,
            },
            ToolCallPerfSample {
                total_ms: 3,
                queue_ms: 1,
            },
            ToolCallPerfSample {
                total_ms: 7,
                queue_ms: 2,
            },
            ToolCallPerfSample {
                total_ms: 5,
                queue_ms: 4,
            },
        ]);

        let summary = summarize_tool_calls(&samples);

        assert_eq!(summary.window, 4);
        assert_eq!(summary.p50_total_ms, 5);
        assert_eq!(summary.max_total_ms, 9);
        assert_eq!(summary.p50_queue_ms, 2);
        assert_eq!(summary.max_queue_ms, 5);
    }

    #[test]
    fn perf_tick_skips_empty_drains_but_keeps_real_work_and_drain_counts() {
        for activity in ["drain", "watcher", "semantic"] {
            let perf = PerfMetrics::default();
            perf.reporter.lock().unwrap().last_report = Instant::now() - perf_tick_interval();
            perf.drain_slices.store(1, Ordering::Relaxed);
            if activity == "watcher" {
                perf.watcher_ingested.store(2, Ordering::Relaxed);
                perf.watcher_paths.store(1, Ordering::Relaxed);
            } else if activity == "semantic" {
                perf.semantic_collects.store(1, Ordering::Relaxed);
                perf.semantic_files.store(2, Ordering::Relaxed);
                perf.semantic_chunks.store(3, Ordering::Relaxed);
                perf.semantic_ms.store(4, Ordering::Relaxed);
            }
            let (_, lines) = capture_log_lines(|| report_perf_tick(&perf, None, (0, 0)));
            if activity == "drain" {
                assert!(lines.is_empty(), "{lines:?}");
            } else {
                assert_eq!(lines.len(), 1, "{lines:?}");
                assert!(lines[0].starts_with("INFO perf tick:"), "{lines:?}");
                assert!(lines[0].contains(" drains=1 "), "{lines:?}");
                let expected = if activity == "watcher" {
                    "watcher={ingested:2,paths:1,dropped:0}"
                } else {
                    "semantic={collects:1,files:2,chunks:3,ms:4}"
                };
                assert!(lines[0].contains(expected), "{lines:?}");
            }
            assert_eq!(perf.drain_slices.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn tool_call_tick_labels_interval_count_and_rolling_window() {
        let samples = VecDeque::from([ToolCallPerfSample {
            total_ms: 3_000,
            queue_ms: 2_900,
        }]);
        let summary = summarize_tool_calls(&samples);

        assert_eq!(
            format_tool_call_summary(1, summary),
            "toolcall={new:1,window:1,p50_total_ms:3000,max_total_ms:3000,p50_queue_ms:2900,max_queue_ms:2900}"
        );
        assert_eq!(
            format_tool_call_summary(0, summary),
            "toolcall={new:0,window:1,p50_total_ms:3000,max_total_ms:3000,p50_queue_ms:2900,max_queue_ms:2900}"
        );
    }

    fn index_event_matches_grammar(line: &str) -> bool {
        let Some(rest) = line.strip_prefix("index_event ") else {
            return false;
        };
        if rest.is_empty() {
            return false;
        }
        rest.split(' ').all(|token| {
            let Some((key, value)) = token.split_once('=') else {
                return false;
            };
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && !value.is_empty()
                && !value.contains(' ')
                && !value.contains('=')
        })
    }

    fn index_event_fields(line: &str) -> BTreeMap<String, String> {
        let mut fields = BTreeMap::new();
        for token in line.split_whitespace().skip(1) {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            fields.insert(key.to_string(), value.to_string());
        }
        fields
    }

    fn assert_index_event_grammar(lines: &[String]) {
        for line in lines {
            assert!(
                index_event_matches_grammar(line),
                "index_event line failed grammar: {line}"
            );
        }
    }

    fn event_matches(line: &str, plane: &str, root: &str, key: &str) -> bool {
        let fields = index_event_fields(line);
        fields.get("plane").map(String::as_str) == Some(plane)
            && fields.get("root").map(String::as_str) == Some(root)
            && fields.get("key").map(String::as_str) == Some(key)
    }

    fn assert_lifecycle_sequence(
        lines: &[String],
        plane: &str,
        root: &str,
        key: &str,
        require_progress: bool,
    ) {
        let events: Vec<_> = lines
            .iter()
            .filter(|line| event_matches(line, plane, root, key))
            .cloned()
            .collect();
        assert_index_event_grammar(&events);
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|line| index_event_fields(line).get("kind").cloned())
            .filter(|kind| {
                matches!(
                    kind.as_str(),
                    "build_started" | "build_progress" | "build_ready"
                )
            })
            .collect();
        assert!(
            kinds.first().is_some_and(|kind| kind == "build_started"),
            "expected build_started first, got {kinds:?} from {events:?}"
        );
        if require_progress {
            assert!(
                kinds.iter().any(|kind| kind == "build_progress"),
                "expected build_progress in {kinds:?} from {events:?}"
            );
        }
        assert!(
            kinds.last().is_some_and(|kind| kind == "build_ready"),
            "expected build_ready last, got {kinds:?} from {events:?}"
        );
        let build_ids: Vec<_> = events
            .iter()
            .filter_map(|line| index_event_fields(line).get("build_id").cloned())
            .collect();
        assert!(!build_ids.is_empty());
        assert!(
            build_ids.iter().all(|id| id == &build_ids[0]),
            "build_id drifted across events: {build_ids:?}"
        );
        for line in &events {
            let fields = index_event_fields(line);
            assert_eq!(fields.get("root").map(String::as_str), Some(root), "{line}");
            assert_eq!(fields.get("key").map(String::as_str), Some(key), "{line}");
        }
    }

    fn tiny_project(name: &str, file_name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join(name);
        std::fs::create_dir_all(&root).expect("create project");
        std::fs::write(root.join(file_name), contents).expect("write fixture");
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        (temp, root)
    }

    #[test]
    fn watcher_rescan_event_reports_reason_cost_rss_and_interval_count() {
        let (_, lines) = capture_index_events(|| {
            log_watcher_rescan(
                Path::new("/tmp/watcher root"),
                RescanReason::KernelDropped,
                42,
                Some(-4096),
                137,
            );
        });
        assert_index_event_grammar(&lines);
        assert_eq!(lines.len(), 1);
        let fields = index_event_fields(&lines[0]);
        assert_eq!(
            fields.get("kind").map(String::as_str),
            Some("watcher_rescan")
        );
        assert_eq!(fields.get("plane").map(String::as_str), Some("watcher"));
        assert_eq!(
            fields.get("root").map(String::as_str),
            Some("/tmp/watcher_root")
        );
        assert_eq!(
            fields.get("reason").map(String::as_str),
            Some("kernel_dropped")
        );
        assert_eq!(fields.get("cost_ms").map(String::as_str), Some("42"));
        assert_eq!(
            fields.get("rss_delta_bytes").map(String::as_str),
            Some("-4096")
        );
        assert_eq!(
            fields.get("raw_events_since_last").map(String::as_str),
            Some("137")
        );
    }

    #[test]
    fn index_event_grammar_rejects_spaces_and_equals_in_values() {
        let (result, lines) = capture_index_events(|| {
            log_index_event(
                IndexEvent::new(
                    IndexEventKind::BuildStarted,
                    IndexPlane::Search,
                    "b-1-1",
                    "/tmp/root",
                    "abc123",
                )
                .field("stage", "streaming"),
            );
        });
        let _ = result;
        assert_index_event_grammar(&lines);
        // The capture slot is process-wide: a parallel lib test that builds an
        // index can emit into the same window. Only this test's own event
        // (unique build_id) is the subject here.
        let own: Vec<&String> = lines
            .iter()
            .filter(|line| {
                index_event_fields(line).get("build_id").map(String::as_str) == Some("b-1-1")
            })
            .collect();
        assert_eq!(own.len(), 1, "{lines:?}");
    }

    #[test]
    fn index_event_keeps_required_fields_for_long_root() {
        let root = format!("/{}", "a".repeat(399));
        let (_, lines) = capture_index_events(|| {
            log_index_event(IndexEvent::new(
                IndexEventKind::BuildStarted,
                IndexPlane::Search,
                "b-9-9",
                &root,
                "keepkey",
            ));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        let fields = index_event_fields(&lines[0]);
        assert_eq!(fields.get("build_id").map(String::as_str), Some("b-9-9"));
        assert_eq!(fields.get("key").map(String::as_str), Some("keepkey"));
        assert!(fields.get("root").is_some(), "{lines:?}");
        assert_index_event_grammar(&lines);
    }

    #[test]
    fn index_event_long_root_keeps_measurements_within_cap() {
        let suffix = "/typescript-eslint/worker";
        let root = format!("/{}{suffix}", "a".repeat(300 - 1 - suffix.len()));
        assert_eq!(root.len(), 300);
        let event = IndexEvent::new(
            IndexEventKind::FirstQuery,
            IndexPlane::Semantic,
            "b-12345-123",
            &root,
            "workspace",
        )
        .field("tool", "semantic_search")
        .field("queue_ms", 12)
        .field("service_ms", 34)
        .field("status", "ok")
        .field("elapsed_ms", 5678)
        .field("ready_to_first_query_ms", 9012)
        .field("completed", 345)
        .field("total", 678);
        let line = format_index_event_line(&event);
        assert!(line.len() <= INDEX_EVENT_MAX_BYTES, "{line}");
        assert!(index_event_matches_grammar(&line), "{line}");
        let fields = index_event_fields(&line);
        for (key, value) in [
            ("elapsed_ms", "5678"),
            ("ready_to_first_query_ms", "9012"),
            ("completed", "345"),
            ("total", "678"),
            ("queue_ms", "12"),
            ("service_ms", "34"),
        ] {
            assert_eq!(fields.get(key).map(String::as_str), Some(value), "{line}");
        }
        assert!(fields["root"].ends_with(suffix), "{line}");
        assert_eq!(
            line,
            include_str!("../../../scripts/telemetry/fixtures/index-event-long-root.txt").trim()
        );
    }

    #[test]
    fn index_event_measurements_take_priority_over_long_descriptions() {
        let event = IndexEvent::new(
            IndexEventKind::FirstQuery,
            IndexPlane::Search,
            "b-1-1",
            "/work/repo",
            "0123456789abcdef",
        )
        .field("tool", "x".repeat(190))
        .field("status", "x".repeat(300))
        .field("ready_to_first_query_ms", 42);
        let line = format_index_event_line(&event);
        assert!(line.len() <= INDEX_EVENT_MAX_BYTES, "{line}");
        assert_eq!(index_event_fields(&line)["ready_to_first_query_ms"], "42");
    }

    #[test]
    fn shortened_index_roots_are_distinct_and_bounded() {
        let root = format!("/{}/typescript-eslint/worker", "a".repeat(300));
        let other = root.replacen('a', "b", 1);
        let shortened = left_truncate_root(&root, 50);
        assert!(shortened.starts_with(".../"));
        assert!(shortened.ends_with("/typescript-eslint/worker"));
        assert_ne!(shortened, left_truncate_root(&other, 50));
        for budget in 0..50 {
            assert!(left_truncate_root(&root, budget).len() <= budget);
        }
    }

    #[test]
    fn first_query_fires_once_per_build_id() {
        let root = PathBuf::from("/tmp/first-query-root");
        let key = "firstquerykey";
        let build_id = "b-1-99";
        let (_, lines) = capture_index_events(|| {
            log_index_event(IndexEvent::new(
                IndexEventKind::BuildReady,
                IndexPlane::Search,
                build_id,
                &root,
                key,
            ));
            assert!(claim_first_query(
                IndexPlane::Search,
                &root,
                "grep",
                1,
                2,
                "ok"
            ));
            assert!(!claim_first_query(
                IndexPlane::Search,
                &root,
                "grep",
                1,
                2,
                "ok"
            ));
        });
        let first_query: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("kind=first_query"))
            .collect();
        assert_eq!(first_query.len(), 1, "{lines:?}");
        let fields = index_event_fields(first_query[0]);
        assert_eq!(fields.get("build_id").map(String::as_str), Some(build_id));
        assert_eq!(fields.get("tool").map(String::as_str), Some("grep"));
    }

    #[test]
    fn semantic_cold_build_emits_started_progress_ready() {
        let (_temp, root) = tiny_project("sem", "lib.rs", "pub fn hello() {}\n");
        let key = crate::root_identity::key::artifact_cache_key(&root);
        let files = vec![root.join("lib.rs")];
        let mut embed = |texts: Vec<String>| {
            Ok(texts
                .into_iter()
                .map(|_| vec![0.01_f32; 384])
                .collect::<Vec<_>>())
        };
        let (built, lines) = capture_index_events(|| {
            crate::semantic_index::SemanticIndex::build(&root, &files, &mut embed, 8)
        });
        built.expect("semantic build");
        assert_lifecycle_sequence(&lines, "semantic", &normalize_index_root(&root), &key, true);
    }

    #[test]
    fn tier2_category_emits_started_ready() {
        let (_temp, root) = tiny_project("t2", "mod0.ts", "export function f0() { return 0; }\n");
        let key = crate::root_identity::key::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        let inspect_dir = _temp.path().join("inspect");
        let manager = std::sync::Arc::new(crate::inspect::InspectManager::new());
        let snapshot = crate::inspect::InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            std::sync::Arc::new(crate::config::Config {
                project_root: Some(root.clone()),
                ..crate::config::Config::default()
            }),
            std::sync::Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        );
        let (outcome, lines) = capture_index_events(|| {
            manager.tier2_run_with_reuse_blocking_fresh(
                snapshot,
                crate::inspect_category::InspectCategory::Complexity,
                crate::inspect::JobScope::for_project(root.clone()),
            )
        });
        assert!(outcome.payload().is_some(), "{outcome:?}");
        assert_lifecycle_sequence(&lines, "tier2", &normalize_index_root(&root), &key, false);
        assert!(
            !lines.iter().any(|line| {
                event_matches(line, "tier2", &normalize_index_root(&root), &key)
                    && line.contains("kind=build_progress")
            }),
            "tier2 must not emit synthetic build_progress: {lines:?}"
        );
    }
}

#[cfg(test)]
mod stderr_mirror;
