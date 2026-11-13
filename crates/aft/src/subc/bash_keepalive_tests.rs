use super::*;

const SESSION: &str = "keepalive-wait";
const ROUTE: RouteChannel = RouteChannel {
    channel: 7,
    epoch: 3,
};

struct WaitHarness {
    dir: tempfile::TempDir,
    ctx: Arc<AppContext>,
    clock: super::super::keepalive::TestClock,
    _active: ActiveToolCalls,
    frames: mpsc::Receiver<WriterFrame>,
    done: mpsc::Receiver<BashDeferredCompletion>,
    metrics: Arc<DispatchPathMetrics>,
    task_id: String,
}

impl WaitHarness {
    async fn start(timeout: u64, reply_window: Duration) -> Self {
        let (dir, root) = super::super::test_support::test_root("bash-keepalive");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.storage_dir = Some(dir.path().join("storage"));
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let metrics = Arc::new(DispatchPathMetrics::new());
        let active = Arc::default();
        let clock = super::super::keepalive::TestClock::new();
        let (writer, frames) = mpsc::channel(32);
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            ROUTE.channel,
            ROUTE.epoch,
            clock.corr,
            vec![],
        )
        .unwrap();
        let progress = super::super::keepalive::channel(&writer, &metrics, &active, &frame, true);
        let (tx, done) = mpsc::channel(8);
        let (touch, _touch_rx) = mpsc::channel(8);
        let now = Instant::now();
        submit_deferred_bash(
            false,
            &executor,
            &tx,
            &touch,
            &metrics,
            |req, ctx| crate::commands::bash::handle(&req, ctx),
            root,
            dir.path().into(),
            SESSION.into(),
            "silent-wait".into(),
            ROUTE,
            clock.corr,
            control_flags(),
            PROTOCOL_VERSION,
            json!({"command":format!("{}; exit 7", crate::test_gate::gate("release", "sleep 0.02")), "wait":true, "timeout":timeout}),
            Default::default(),
            BashWaitCancel {
                keepalive: progress,
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: Default::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            true,
            super::remote_policy::RemoteSource::None,
            crate::commands::blocking_wait::ReplyBudget::with_deadline(now, now + reply_window),
        );
        let task_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(task) = ctx.bash_background().list(0).first() {
                    break task.info.task_id.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child registered");
        Self {
            dir,
            ctx,
            clock,
            _active: active,
            frames,
            done,
            metrics,
            task_id,
        }
    }

    async fn tick(&mut self, at: u64) {
        self.clock.now.store(at, Ordering::SeqCst);
        let push = tokio::time::timeout(Duration::from_secs(5), self.frames.recv())
            .await
            .expect("running observation must keep the silent call alive")
            .unwrap();
        assert_eq!(push.frame().header.ty, FrameType::Push);
        assert_eq!(push.frame().header.corr, self.clock.corr);
        assert_eq!(push.frame().body, br#"{"kind":"keepalive"}"#);
    }

    async fn quiet(&mut self) {
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(250), self.frames.recv()).await,
            Ok(Some(_))
        ));
    }

    async fn result(&mut self) -> Response {
        let done = tokio::time::timeout(Duration::from_secs(8), self.done.recv())
            .await
            .unwrap()
            .unwrap();
        self.metrics.held_bash_calls.remove(ROUTE, self.clock.corr);
        done.result.unwrap().response
    }
}

impl Drop for WaitHarness {
    fn drop(&mut self) {
        let _ = self.ctx.bash_background().kill(&self.task_id, SESSION);
    }
}

#[tokio::test]
async fn bash_keepalive_silent_foreground_survives_three_quiet_windows() {
    let mut h = WaitHarness::start(30_000, Duration::from_secs(25)).await;
    // Only the keepalive clock advances: the real child stays silent and gated
    // until nine 30-second supervision intervals have been observed.
    for at in (30_000..=270_000).step_by(30_000) {
        h.tick(at).await;
    }
    assert!(h.done.try_recv().is_err());
    std::fs::write(h.dir.path().join("release"), "").unwrap();
    let response = h.result().await;
    assert_eq!(response.data["exit_code"], 7, "{response:?}");
    assert_eq!(response.data["status"], "failed", "{response:?}");
    h.clock.now.store(300_000, Ordering::SeqCst);
    h.quiet().await;
}

#[tokio::test]
async fn bash_keepalive_stalled_state_observation_goes_silent() {
    let mut h = WaitHarness::start(30_000, Duration::from_secs(25)).await;
    h.tick(30_000).await;
    let task = h.ctx.bash_background().task_for_test(&h.task_id).unwrap();
    let state = task.state.lock().unwrap();
    h.quiet().await;
    h.clock.now.store(150_000, Ordering::SeqCst);
    h.quiet().await;
    drop(state);
    h.tick(150_000).await;
    std::fs::write(h.dir.path().join("release"), "").unwrap();
    assert_eq!(h.result().await.data["exit_code"], 7);
}

#[tokio::test]
async fn bash_keepalive_does_not_extend_command_timeout() {
    let started = Instant::now();
    let mut h = WaitHarness::start(1_500, Duration::from_secs(15)).await;
    for at in [30_000, 60_000, 90_000] {
        h.tick(at).await;
    }
    let response = h.result().await;
    assert_eq!(response.data["status"], "timed_out", "{response:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn bash_keepalive_does_not_extend_reply_deadline() {
    let started = Instant::now();
    let mut h = WaitHarness::start(30_000, Duration::from_secs(3)).await;
    for at in [30_000, 60_000, 90_000] {
        h.tick(at).await;
    }
    let response = h.result().await;
    assert_eq!(response.data["task_id"], h.task_id, "{response:?}");
    assert_eq!(response.data["status"], "running", "{response:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
    h.clock.now.store(120_000, Ordering::SeqCst);
    h.quiet().await;
}
