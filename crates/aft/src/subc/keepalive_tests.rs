use super::*;

static JOBS: LazyLock<std::sync::Mutex<HashMap<u64, TestJob>>> = LazyLock::new(Default::default);
static CLOCKS: LazyLock<std::sync::Mutex<HashMap<u64, Arc<AtomicU64>>>> =
    LazyLock::new(Default::default);
static NEXT: AtomicU64 = AtomicU64::new(800_000);

pub(super) fn clock(corr: u64) -> Option<Arc<AtomicU64>> {
    CLOCKS.lock().unwrap().get(&corr).cloned()
}

struct TestJob {
    steps: std::sync::mpsc::Receiver<(u64, bool)>,
    ack: mpsc::UnboundedSender<()>,
    clock: Arc<AtomicU64>,
    cap: u64,
    stopped: Arc<AtomicBool>,
}

fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    if req.command == "configure" {
        return crate::commands::configure::handle_configure(&req, ctx);
    }
    let corr = req.params["test_corr"].as_u64().unwrap();
    let job = JOBS.lock().unwrap().remove(&corr).unwrap();
    let token = crate::executor::current_job_cancellation().unwrap();
    let _ = job.ack.send(());
    while !token.cancel_requested_before_commit() {
        match job.steps.recv_timeout(Duration::from_millis(5)) {
            Ok((at, progress)) => {
                job.clock.store(at, Ordering::SeqCst);
                if at >= job.cap {
                    break;
                }
                if progress {
                    crate::run_tool_call::progress::report();
                }
                let _ = job.ack.send(());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    job.stopped.store(true, Ordering::SeqCst);
    Response::success(req.id, json!({"stopped": true}))
}

struct Harness {
    daemon: tokio::io::DuplexStream,
    module: std::thread::JoinHandle<Result<(), SubcError>>,
    _dir: tempfile::TempDir,
    corr: u64,
    steps: std::sync::mpsc::Sender<(u64, bool)>,
    ack: mpsc::UnboundedReceiver<()>,
    stopped: Arc<AtomicBool>,
}

impl Harness {
    async fn start(support: bool, cap: u64) -> Self {
        let corr = NEXT.fetch_add(1, Ordering::SeqCst);
        let (dir, root) = test_support::test_root("call-keepalive");
        let user = test_support::bind_config_without_indexes(dir.path());
        let storage = dir.path().join("storage");
        let (mut daemon, module) = tokio::io::duplex(128 * 1024);
        let (read, write) = tokio::io::split(module);
        let module = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_module_loop(
                    read,
                    write,
                    &storage.join("connection.json"),
                    App::default_shared(),
                    Arc::new(Executor::new()),
                    dispatch,
                    Some(user),
                    false,
                    usize::MAX,
                    None,
                    &storage,
                    QuiescedLsp::ReapInBackground,
                    None,
                    None,
                ))
        });
        assert_eq!(
            read_frame(&mut daemon).await.unwrap().unwrap().header.ty,
            FrameType::Hello
        );
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::HelloAck,
                control_flags(),
                0,
                0,
                HELLO_CORR,
                b"{}".to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let bind = ModuleControlRequest::RouteBind {
            route_channel: 7,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(
                root.as_path(),
                "opencode",
                "call-keepalive",
            ),
            principal: Some(Principal::Direct),
            consumer_capabilities: support
                .then(|| vec![cortexkit_role_tool_provider::CALL_KEEPALIVE_CAPABILITY.to_owned()]),
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        };
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr + 1_000_000,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut daemon))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if frame.header.corr == corr + 1_000_000 {
                assert_eq!(frame.header.ty, FrameType::Response, "{frame:?}");
                break;
            }
        }
        let clock = Arc::new(AtomicU64::new(0));
        CLOCKS.lock().unwrap().insert(corr, clock.clone());
        let (steps, receiver) = std::sync::mpsc::channel();
        let (sender, mut ack) = mpsc::unbounded_channel();
        let stopped = Arc::new(AtomicBool::new(false));
        JOBS.lock().unwrap().insert(
            corr,
            TestJob {
                steps: receiver,
                ack: sender,
                clock,
                cap,
                stopped: stopped.clone(),
            },
        );
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::Request,
                control_flags(),
                7,
                1,
                corr,
                serde_json::to_vec(
                    &json!({"name":"grep", "arguments":{"pattern":"x", "test_corr":corr}}),
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), ack.recv())
            .await
            .unwrap()
            .expect("job started");
        Self {
            daemon,
            module,
            _dir: dir,
            corr,
            steps,
            ack,
            stopped,
        }
    }

    async fn step(&mut self, at: u64, progress: bool) {
        self.steps.send((at, progress)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.ack.recv())
            .await
            .unwrap()
            .expect("job advanced");
    }

    async fn next(&mut self) -> Frame {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.daemon))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if frame.header.corr == self.corr && frame.header.channel == 7 {
                return frame;
            }
        }
    }

    async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(30), self.next())
                .await
                .is_err(),
            "idle work emitted a call frame"
        );
    }

    async fn finish(mut self) {
        drop(self.steps);
        write_frame(
            &mut self.daemon,
            &Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap(),
        )
        .await
        .unwrap();
        while tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.daemon))
            .await
            .unwrap()
            .unwrap()
            .is_some()
        {}
        drop(self.daemon);
        self.module.join().unwrap().unwrap();
        CLOCKS.lock().unwrap().remove(&self.corr);
    }
}

#[tokio::test]
async fn progressing_call_keepalive_survives_runner_quiet_window() {
    let mut harness = Harness::start(true, u64::MAX).await;
    let mut last_push = 0;
    // The fake runner's quiet window is 90 seconds. Work stays outstanding
    // for three windows; only a PUSH on this exact call renews its wait.
    for at in (30_000..=270_000).step_by(30_000) {
        assert!(at - last_push < 90_000);
        harness.step(at, true).await;
        let frame = harness.next().await;
        assert_eq!(frame.header.ty, FrameType::Push);
        assert_eq!(frame.header.epoch, 1);
        assert_eq!(frame.body, br#"{"kind":"keepalive"}"#);
        last_push = at;
    }
    assert_eq!(last_push, 270_000);
    harness.finish().await;
}

#[tokio::test]
async fn stalled_call_keepalive_goes_silent() {
    let mut harness = Harness::start(true, u64::MAX).await;
    harness.step(30_000, true).await;
    assert_eq!(harness.next().await.header.ty, FrameType::Push);
    for at in [60_000, 90_000, 120_000, 150_000] {
        harness.step(at, false).await;
        harness.quiet().await;
    }
    harness.finish().await;
}

#[tokio::test]
async fn call_keepalive_is_rate_limited_and_bind_opt_in() {
    for support in [true, false] {
        let mut harness = Harness::start(support, u64::MAX).await;
        for at in [1, 29_999, 30_000, 30_001, 59_999, 60_000] {
            harness.step(at, true).await;
            if support && matches!(at, 30_000 | 60_000) {
                assert_eq!(harness.next().await.header.ty, FrameType::Push);
            } else {
                harness.quiet().await;
            }
        }
        harness.finish().await;
    }
}

#[tokio::test]
async fn call_keepalive_does_not_extend_tool_cap() {
    let mut harness = Harness::start(true, 120_000).await;
    for at in [30_000, 60_000, 90_000] {
        harness.step(at, true).await;
        assert_eq!(harness.next().await.header.ty, FrameType::Push);
    }
    harness.steps.send((120_000, true)).unwrap();
    let terminal = harness.next().await;
    assert_eq!(terminal.header.ty, FrameType::Response);
    assert!(harness.stopped.load(Ordering::SeqCst));
    harness.quiet().await;
    harness.finish().await;
}

#[tokio::test]
async fn runner_cancel_stops_call_keepalive_job_and_sends_terminal() {
    let mut harness = Harness::start(true, u64::MAX).await;
    harness.step(30_000, true).await;
    assert_eq!(harness.next().await.header.ty, FrameType::Push);
    let cancel = Frame::build(
        FrameType::Cancel,
        control_flags(),
        7,
        1,
        harness.corr,
        Vec::new(),
    )
    .unwrap();
    write_frame(&mut harness.daemon, &cancel).await.unwrap();
    let terminal = harness.next().await;
    assert_eq!(terminal.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&terminal.body).unwrap()["code"],
        "request_cancelled"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !harness.stopped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    harness.quiet().await;
    harness.finish().await;
}
