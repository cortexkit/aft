use super::*;

struct Daemon {
    socket: tokio::io::DuplexStream,
    module: std::thread::JoinHandle<Result<ModuleLoopExit, SubcError>>,
    executor: Arc<Executor>,
}

impl Daemon {
    async fn start(root: &Path, ctx: &Arc<AppContext>, ops: &[&str], dispatch: DispatchFn) -> Self {
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(ProjectRootId::from_path(root).unwrap(), ctx.clone()));
        let (socket, module) = tokio::io::duplex(128 * 1024);
        let root = root.to_owned();
        let app = ctx.app().clone();
        let workers = executor.clone();
        let module = std::thread::Builder::new()
            .stack_size(MODULE_RUNTIME_STACK_BYTES)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let (read, write) = tokio::io::split(module);
                runtime.block_on(run_module_loop(
                    read,
                    write,
                    &root.join("missing-subc.json"),
                    app,
                    workers,
                    dispatch,
                    Some(test_support::bind_config_without_indexes(&root)),
                    false,
                    MAX_FRAME_BODY_LEN as usize,
                    None,
                    &root.join("storage"),
                    QuiescedLsp::ReapInBackground,
                    None,
                    None,
                    None,
                ))
            })
            .unwrap();
        let mut daemon = Self {
            socket,
            module,
            executor,
        };
        assert_eq!(daemon.recv().await.header.ty, FrameType::Hello);
        daemon
            .send(FrameType::HelloAck, 0, HELLO_CORR, json!({"subc_ops": ops}))
            .await;
        daemon
    }

    async fn send(&mut self, ty: FrameType, channel: u16, corr: u64, body: Value) {
        let epoch = u32::from(channel != 0);
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        let frame = Frame::build(ty, control_flags(), channel, epoch, corr, body).unwrap();
        write_frame(&mut self.socket, &frame).await.unwrap();
    }

    async fn recv(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(10), read_frame(&mut self.socket))
            .await
            .expect("daemon did not receive a frame")
            .unwrap()
            .unwrap()
    }

    async fn reply(&mut self, channel: u16, corr: u64) -> Frame {
        loop {
            let frame = self.recv().await;
            if frame.header.channel == channel && frame.header.corr == corr {
                return frame;
            }
        }
    }

    async fn bind(&mut self, root: &Path, channel: u16) {
        self.bind_with_role(root, channel, true).await;
    }

    async fn bind_with_role(&mut self, root: &Path, channel: u16, provider_v1: bool) {
        let corr = u64::from(channel) + 100;
        let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new("opencode", "edge-lifecycle"),
            principal: Some(Principal::Direct),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: provider_v1
                .then(|| std::collections::BTreeMap::from([("tool-provider".into(), "v1".into())])),
            call_mode: None,
        })
        .unwrap();
        bind["identity"]["project_root"] = json!(root);
        self.send(FrameType::Request, 0, corr, bind).await;
        let ack = self.reply(0, corr).await;
        assert!(matches!(
            serde_json::from_slice(&ack.body).unwrap(),
            ModuleControlResponse::RouteBindAck {}
        ));
    }

    async fn finish(mut self) {
        self.send(FrameType::Goodbye, 0, 0, Value::Null).await;
        drop(self.socket);
        self.module.join().unwrap().unwrap();
        for (root, ctx) in self.executor.actor_entries() {
            ctx.bash_background().shutdown();
            ctx.clear_db();
            self.executor.remove_actor(&root);
        }
    }
}

async fn hold_lane(
    executor: &Executor,
    root: &ProjectRootId,
    lane: Lane,
) -> std::sync::mpsc::Sender<()> {
    let (entered, ready) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    drop(executor.submit_async(
        root.clone(),
        lane,
        "hold-edge-lane".into(),
        Box::new(move |_| {
            let _ = entered.send(());
            let _ = held.recv_timeout(Duration::from_secs(20));
            Response::success("hold-edge-lane", json!({}))
        }),
    ));
    ready.await.unwrap();
    release
}

fn dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    match request.command.as_str() {
        "configure" => crate::commands::configure::handle_configure(&request, ctx),
        "bash" => crate::commands::bash::handle(&request, ctx),
        _ => panic!("unexpected tool dispatch: {}", request.command),
    }
}

#[tokio::test]
async fn route_close_retires_admission_before_dispatch() {
    let (_dir, root) = test_support::test_root("close-ledger-admission");
    let ctx = test_support::test_ctx();
    let mut daemon = Daemon::start(root.as_path(), &ctx, &[], dispatch).await;
    daemon.bind(root.as_path(), 1).await;
    daemon.bind(root.as_path(), 2).await;
    ctx.wait_for_database_settled(Instant::now() + Duration::from_secs(10))
        .await;
    let db = ctx
        .db()
        .expect("both binds must share the configured ledger");
    let release = hold_lane(&daemon.executor, &root, Lane::CallLedger).await;
    let call = json!({"name":"read", "arguments":{"path":"README.md"}, "call_key":"same-read"});
    daemon.send(FrameType::Request, 1, 10, call.clone()).await;
    let executor = daemon.executor.clone();
    tokio::select! {
        early = daemon.reply(1, 10) => panic!("unexpected early admission reply: {:?} {}", early.header.ty, String::from_utf8_lossy(&early.body)),
        queued = tokio::time::timeout(Duration::from_secs(5), async {
            while executor.queued_lane_for_test(&root, "ledger-1-10") != Some(Lane::CallLedger) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }) => queued.expect("keyed admission never reached the held ledger lane"),
    }
    daemon.send(FrameType::Goodbye, 1, 0, Value::Null).await;
    daemon.send(FrameType::Ping, 0, 1010, Value::Null).await;
    let mut released = 0;
    loop {
        let frame = daemon.recv().await;
        if frame.header.channel == 1 && frame.header.corr == 10 {
            assert_eq!(frame.header.ty, FrameType::Error);
            assert_eq!(
                serde_json::from_slice::<Value>(&frame.body).unwrap()["code"],
                "route_released"
            );
            released += 1;
        }
        if frame.header.ty == FrameType::Pong && frame.header.corr == 1010 {
            break;
        }
    }
    daemon.send(FrameType::Request, 2, 20, call).await;
    release.send(()).unwrap();
    let repeat = loop {
        let frame = daemon.recv().await;
        if frame.header.channel == 1 && frame.header.corr == 10 {
            assert_eq!(frame.header.ty, FrameType::Error);
            assert_eq!(
                serde_json::from_slice::<Value>(&frame.body).unwrap()["code"],
                "route_released"
            );
            released += 1;
        }
        if frame.header.channel == 2 && frame.header.corr == 20 {
            break frame;
        }
    };
    assert_eq!(repeat.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&repeat.body).unwrap()["code"],
        "not_started"
    );
    let conn = db.lock().unwrap();
    let (state, outcome): (String, String) = conn
        .query_row(
            "SELECT state,outcome FROM call_ledger WHERE call_key='same-read'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);
    assert_eq!(
        (state.as_str(), outcome.as_str()),
        ("Settled", "not_started")
    );
    let extra = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            let frame = daemon.recv().await;
            if frame.header.channel == 1 && frame.header.corr == 10 {
                assert_eq!(
                    serde_json::from_slice::<Value>(&frame.body).unwrap()["code"],
                    "route_released"
                );
                released += 1;
            }
            if frame.header.channel == 2 && frame.header.corr == 20 {
                break;
            }
        }
    })
    .await;
    assert!(extra.is_err(), "repeat received a second terminal");
    assert_eq!(
        released, 1,
        "the closed route must receive exactly one terminal"
    );
    daemon.finish().await;
}

#[cfg(unix)]
#[tokio::test]
async fn retired_shell_does_not_block_pong_or_unrelated_bind() {
    let (project, root) = test_support::test_root("retired-shell");
    let (other, _) = test_support::test_root("unrelated-bind");
    let ctx = test_support::test_ctx();
    let mut daemon = Daemon::start(root.as_path(), &ctx, &[], dispatch).await;
    daemon.bind(root.as_path(), 1).await;
    daemon.bind(root.as_path(), 2).await;
    ctx.wait_for_database_settled(Instant::now() + Duration::from_secs(10))
        .await;
    assert!(
        ctx.db().is_some(),
        "the foreground shell needs the configured ledger"
    );
    let started = project.path().join("started");
    let stop = project.path().join("stop");
    daemon.send(FrameType::Request, 1, 10, json!({
        "name":"bash", "call_key":"detaching-shell", "arguments":{
            "command":format!("printf x > '{}'; for i in $(seq 1 200); do [ -e '{}' ] && break; sleep 0.05; done", started.display(), stop.display()),
        },
    })).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("foreground keyed shell did not start");
    let release = hold_lane(&daemon.executor, &root, Lane::PureRead).await;
    daemon.send(FrameType::Goodbye, 1, 0, Value::Null).await;
    daemon.send(FrameType::Ping, 0, 1010, Value::Null).await;
    let edge_replies = tokio::time::timeout(Duration::from_secs(2), async {
        assert_eq!(daemon.reply(0, 1010).await.header.ty, FrameType::Pong);
        daemon.bind(other.path(), 3).await;
    })
    .await;
    release.send(()).unwrap();
    std::fs::write(stop, b"done").unwrap();
    daemon.finish().await;
    edge_replies.expect("root A's epoch reader must not delay the Pong or root B's bind");
}

#[tokio::test]
async fn cancel_of_unrelated_corr_keeps_bg_events_stream_open() {
    let (_dir, root) = test_support::test_root("bg-events-cancel-correlation");
    let ctx = test_support::test_ctx();
    let mut daemon = Daemon::start(root.as_path(), &ctx, &[], dispatch).await;
    daemon.bind_with_role(root.as_path(), 1, false).await;
    daemon.bind_with_role(root.as_path(), 2, false).await;
    ctx.wait_for_database_settled(Instant::now() + Duration::from_secs(10))
        .await;
    // A second subscribe must end the first. This proves installation rather
    // than treating the lack of a StreamEnd as proof of a working subscription.
    daemon
        .send(FrameType::Request, 1, 9, json!({"op":"bg_events"}))
        .await;
    daemon
        .send(FrameType::Request, 1, 10, json!({"op":"bg_events"}))
        .await;
    loop {
        let frame = daemon.reply(1, 9).await;
        if frame.header.ty == FrameType::StreamEnd {
            break;
        }
        assert_eq!(frame.header.ty, FrameType::StreamData);
    }
    daemon
        .send(FrameType::Request, 2, 20, json!({"op":"bg_events"}))
        .await;
    daemon.send(FrameType::Cancel, 1, 11, Value::Null).await;
    // Route 2's replacement end comes after the Cancel and anything it sent
    // to the same reliable writer queue. Pong could overtake an already
    // queued end because it uses a separate priority queue.
    daemon
        .send(FrameType::Request, 2, 21, json!({"op":"bg_events"}))
        .await;
    loop {
        let frame = daemon.recv().await;
        match (frame.header.channel, frame.header.corr) {
            (1, 10) => assert_eq!(
                frame.header.ty,
                FrameType::StreamData,
                "a Cancel for another correlation must not end the stream"
            ),
            (2, 20) if frame.header.ty == FrameType::StreamEnd => break,
            (2, 20) => assert_eq!(frame.header.ty, FrameType::StreamData),
            _ => {}
        }
    }
    daemon.send(FrameType::Cancel, 1, 10, Value::Null).await;
    loop {
        let frame = daemon.reply(1, 10).await;
        if frame.header.ty == FrameType::StreamEnd {
            break;
        }
        assert_eq!(frame.header.ty, FrameType::StreamData);
    }
    daemon.finish().await;
}
