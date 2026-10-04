//! Exercise the ConPTY completion policy without depending on a Windows host's
//! scheduling. The reader is held before capturing any command output; channels
//! establish child-exit, capture and EOF ordering without sleeps or retries.

use super::*;
use crate::bash_background::persistence::{read_task, task_paths, write_task};
use crate::bash_background::pty_runtime::{CompletionCoordinator, PtyOutputDrain};
use std::io::{self, Cursor, Read};

const OUTPUT: &str = "café-東京\r\n";
const WAIT: Duration = Duration::from_secs(10);

struct DelayedReader {
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
    output: Cursor<Vec<u8>>,
    held: bool,
    fail: bool,
}

impl Read for DelayedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.held {
            self.held = true;
            self.reached.send(()).unwrap();
            let _ = self.release.recv_timeout(WAIT);
            if self.fail {
                return Err(io::Error::other("injected capture failure"));
            }
        }
        self.output.read(buf)
    }
}

#[derive(Debug)]
struct NoopKiller;

impl portable_pty::ChildKiller for NoopKiller {
    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(Self)
    }
}

struct Fixture {
    registry: BgTaskRegistry,
    task: Arc<BgTask>,
    release: crossbeam_channel::Sender<()>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(fail_reader: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let task_id = random_slug();
        let paths = task_paths(dir.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.dir).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            "session".to_owned(),
            "echo café-東京".to_owned(),
            dir.path().to_path_buf(),
            Some(dir.path().to_path_buf()),
            None,
            true,
            false,
        );
        metadata.mode = BgMode::Pty;
        metadata.status = BgTaskStatus::Running;
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.exit, "0").unwrap();
        let spill = fs::File::create(&paths.pty).unwrap();
        registry
            .insert_rehydrated_task(metadata, paths, false)
            .unwrap();
        let task = registry.task_for_test(&task_id).unwrap();
        let coordinator = Arc::new(CompletionCoordinator::new(
            task_id,
            "session".to_owned(),
            registry.inner.wake_tx.clone(),
        ));
        let reader_done = Arc::new(AtomicBool::new(false));
        let reader_eof = Arc::new(AtomicBool::new(false));
        let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        super::super::pty_process::spawn_reader(
            Box::new(DelayedReader {
                reached: reached_tx,
                release: release_rx,
                output: Cursor::new(OUTPUT.as_bytes().to_vec()),
                held: false,
                fail: fail_reader,
            }),
            spill,
            reader_done.clone(),
            reader_eof.clone(),
            coordinator.clone(),
            None,
        );
        reached_rx
            .recv_timeout(WAIT)
            .expect("reader held before capture");
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        task.state.lock().unwrap().runtime = TaskRuntime::Pty(Some(PtyRuntime {
            master: Some(pair.master),
            writer: Arc::new(Mutex::new(Box::new(io::sink()))),
            killer: Box::new(NoopKiller),
            child_pid: None,
            reader_done,
            reader_eof,
            exit_observed: Arc::new(AtomicBool::new(true)),
            was_killed: Arc::new(AtomicBool::new(false)),
            coordinator: coordinator.clone(),
            output_drain: Some(PtyOutputDrain::default()),
        }));
        drop(pair.slave);
        // The child has exited, but the reader has not captured even one byte.
        coordinator.signal_one_done();
        Self {
            registry,
            task,
            release,
            _dir: dir,
        }
    }

    fn finish_reader(&self) {
        self.release.send(()).unwrap();
        self.registry
            .inner
            .wake_rx
            .recv_timeout(WAIT)
            .expect("reader reached EOF or error");
    }

    fn assert_pending(&self) {
        assert_eq!(self.task.snapshot(0).info.status, BgTaskStatus::Running);
        assert_eq!(
            read_task(&self.task.paths.json).unwrap().status,
            BgTaskStatus::Running
        );
        assert!(self
            .registry
            .pending_completions_for_session("session")
            .is_empty());
        assert_eq!(fs::read(&self.task.paths.pty).unwrap(), b"");
    }

    fn assert_incomplete(&self) {
        let snapshot = self.task.snapshot(0);
        assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
        assert_eq!(
            snapshot.exit_code,
            Some(0),
            "retain the child's actual exit code"
        );
        assert!(snapshot
            .info
            .status_reason
            .as_deref()
            .unwrap()
            .contains("output may be incomplete"));
        let persisted = read_task(&self.task.paths.json).unwrap();
        assert_eq!(persisted.status, snapshot.info.status);
        assert_eq!(persisted.status_reason, snapshot.info.status_reason);
        let completions = self.registry.pending_completions_for_session("session");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].status, BgTaskStatus::Failed);
        assert_eq!(completions[0].status_reason, snapshot.info.status_reason);
    }
}

#[test]
fn conpty_wrapper_marker_does_not_close_before_child_exit() {
    let fixture = Fixture::new(false);
    if let TaskRuntime::Pty(Some(pty)) = &fixture.task.state.lock().unwrap().runtime {
        pty.exit_observed.store(false, Ordering::SeqCst);
    }
    fixture.registry.poll_task(&fixture.task).unwrap();
    fixture.assert_pending();
    if let TaskRuntime::Pty(Some(pty)) = &fixture.task.state.lock().unwrap().runtime {
        assert!(pty.master.is_some());
        assert!(pty.output_drain.as_ref().unwrap().deadline.is_none());
        pty.exit_observed.store(true, Ordering::SeqCst);
    }
    fixture.registry.poll_task(&fixture.task).unwrap();
    fixture.assert_pending();
    fixture.finish_reader();
    fixture.registry.poll_task(&fixture.task).unwrap();
    assert_eq!(
        fixture.task.snapshot(0).info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(fs::read_to_string(&fixture.task.paths.pty).unwrap(), OUTPUT);
}

#[test]
fn conpty_completion_waits_for_output_capture() {
    let fixture = Fixture::new(false);
    fixture.registry.poll_task(&fixture.task).unwrap();
    fixture.assert_pending();
    if let TaskRuntime::Pty(Some(pty)) = &fixture.task.state.lock().unwrap().runtime {
        assert!(
            pty.master.is_none(),
            "close the pseudoconsole before waiting for EOF"
        );
    }
    fixture.finish_reader();
    fixture.registry.poll_task(&fixture.task).unwrap();
    assert_eq!(
        fixture.task.snapshot(0).info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(fs::read_to_string(&fixture.task.paths.pty).unwrap(), OUTPUT);
    assert_eq!(
        fixture
            .registry
            .pending_completions_for_session("session")
            .len(),
        1
    );
}

#[test]
fn conpty_completion_drain_deadline_reports_incomplete_output() {
    let fixture = Fixture::new(false);
    if let TaskRuntime::Pty(Some(pty)) = &mut fixture.task.state.lock().unwrap().runtime {
        // Advance only the drain clock, not wall time or the task's hard kill.
        pty.output_drain.as_mut().unwrap().deadline = Some(Instant::now() - WAIT);
    }
    fixture.registry.poll_task(&fixture.task).unwrap();
    fixture.assert_incomplete();
    fixture.finish_reader();
    fixture.registry.poll_task(&fixture.task).unwrap();
    fixture.assert_incomplete(); // Late EOF must not rewrite the declared outcome.
}

#[test]
fn conpty_completion_reader_error_reports_incomplete_output() {
    let fixture = Fixture::new(true);
    fixture.finish_reader();
    fixture.registry.poll_task(&fixture.task).unwrap();
    fixture.assert_incomplete();
}

#[test]
fn conpty_kill_after_exit_waits_for_output_capture() {
    let fixture = Fixture::new(false);
    fixture
        .registry
        .kill(&fixture.task.task_id, "session")
        .unwrap();
    fixture.assert_pending();
    fixture.finish_reader();
    fixture.registry.poll_task(&fixture.task).unwrap();
    assert_eq!(
        fixture.task.snapshot(0).info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(fs::read_to_string(&fixture.task.paths.pty).unwrap(), OUTPUT);
}
