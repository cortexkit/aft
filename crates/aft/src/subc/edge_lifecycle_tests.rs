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
            role_versions: Some(std::collections::BTreeMap::from([(
                "tool-provider".into(),
                "v1".into(),
            )])),
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
    assert_eq!(
        request.command, "configure",
        "an abandoned tool must not run"
    );
    crate::commands::configure::handle_configure(&request, ctx)
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
    let call = json!({"name":"read", "arguments":{"filePath":"README.md"}, "call_key":"same-read"});
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
