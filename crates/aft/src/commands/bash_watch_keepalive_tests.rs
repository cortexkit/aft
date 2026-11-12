use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

const SESSION: &str = "quiet-watch";

struct QuietWatch {
    dir: tempfile::TempDir,
    ctx: Arc<AppContext>,
    task: String,
    pending: PendingResponse,
    token: crate::executor::JobCancellation,
    clock: Arc<AtomicU64>,
    pushes: std::sync::mpsc::Receiver<()>,
}

impl QuietWatch {
    fn start(reply_window: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(dir.path().into()),
                storage_dir: Some(dir.path().join("storage")),
                experimental_bash_background: true,
                ..Default::default()
            },
        ));
        let spawn: RawRequest = serde_json::from_value(json!({
            "id":"quiet-child", "command":"bash", "session_id":SESSION,
            "params":{"command":format!("{}; exit 7", crate::test_gate::gate("release", "sleep 0.02")), "background":true, "timeout":30_000}
        })).unwrap();
        let launched = crate::commands::bash::handle(&spawn, &ctx);
        assert!(launched.success, "{launched:?}");
        let task = launched.data["task_id"].as_str().unwrap().to_owned();
        let token = crate::executor::JobCancellation::new();
        let clock = Arc::new(AtomicU64::new(0));
        let (tx, pushes) = std::sync::mpsc::channel();
        let origin = Instant::now();
        token.set_progress(crate::run_tool_call::progress::Progress::with_clock(
            {
                let clock = clock.clone();
                move || origin + Duration::from_millis(clock.load(Ordering::SeqCst))
            },
            move |_| tx.send(()).is_ok(),
        ));
        let request: RawRequest = serde_json::from_value(json!({
            "id":"quiet-watch", "command":"bash_watch", "session_id":SESSION,
            "worker_session":true, "params":{"task_id":task}
        }))
        .unwrap();
        let now = Instant::now();
        let outcome = {
            let _installed = crate::executor::install_job_cancellation(token.clone());
            handle_deferred_with_budget(
                &request,
                ctx.clone(),
                crate::commands::blocking_wait::ReplyBudget {
                    received_at: now,
                    deadline: now + reply_window,
                },
            )
        };
        let DispatchOutcome::Deferred(pending) = outcome else {
            panic!("watch should wait")
        };
        Self {
            dir,
            ctx,
            task,
            pending,
            token,
            clock,
            pushes,
        }
    }

    fn tick(&self, at: u64) {
        self.clock.store(at, Ordering::SeqCst);
        self.pushes
            .recv_timeout(Duration::from_secs(5))
            .expect("silent running watch must report supervision");
    }

    fn result(&mut self) -> Response {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(response) = (self.pending.poll)(&self.ctx) {
                return response;
            }
            assert!(Instant::now() < deadline, "watch failed to finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for QuietWatch {
    fn drop(&mut self) {
        self.token.request_cancel();
        let _ = self.ctx.bash_background().kill(&self.task, SESSION);
    }
}

#[test]
fn bash_watch_keepalive_quiet_worker_finishes_with_real_result() {
    let mut h = QuietWatch::start(Duration::from_secs(20));
    for at in (30_000..=270_000).step_by(30_000) {
        h.tick(at);
    }
    assert!((h.pending.poll)(&h.ctx).is_none());
    std::fs::write(h.dir.path().join("release"), "").unwrap();
    let response = h.result();
    assert_eq!(response.data["exit_code"], 7, "{response:?}");
    assert_eq!(response.data["waited"]["reason"], "exited", "{response:?}");
}

#[test]
fn bash_watch_keepalive_stalled_state_read_goes_silent() {
    let mut h = QuietWatch::start(Duration::from_secs(20));
    h.tick(30_000);
    let task = h.ctx.bash_background().task_for_test(&h.task).unwrap();
    let held = task.state.lock().unwrap();
    assert!(h.pushes.recv_timeout(Duration::from_millis(250)).is_err());
    h.clock.store(150_000, Ordering::SeqCst);
    assert!(h.pushes.recv_timeout(Duration::from_millis(250)).is_err());
    drop(held);
    h.tick(150_000);
    std::fs::write(h.dir.path().join("release"), "").unwrap();
    assert_eq!(h.result().data["exit_code"], 7);
}

#[test]
fn bash_watch_keepalive_does_not_extend_reply_deadline() {
    let started = Instant::now();
    let mut h = QuietWatch::start(Duration::from_secs(2));
    for at in [30_000, 60_000, 90_000] {
        h.tick(at);
    }
    let response = h.result();
    assert_eq!(response.data["status"], "running", "{response:?}");
    assert_eq!(response.data["waited"]["reason"], "timeout", "{response:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn bash_watch_cancel_stops_waiting_but_leaves_silent_child_running() {
    let mut h = QuietWatch::start(Duration::from_secs(20));
    h.tick(30_000);
    h.token.request_cancel();
    let response = h.result();
    assert_eq!(response.data["code"], "watch_interrupted", "{response:?}");
    assert_eq!(
        h.ctx
            .bash_background()
            .observed_status(&h.task, SESSION, 0)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Running
    );
    h.clock.store(150_000, Ordering::SeqCst);
    assert!(h.pushes.recv_timeout(Duration::from_millis(250)).is_err());
    std::fs::write(h.dir.path().join("release"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = h
            .ctx
            .bash_background()
            .observed_status(&h.task, SESSION, 0)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(snapshot.exit_code, Some(7));
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
