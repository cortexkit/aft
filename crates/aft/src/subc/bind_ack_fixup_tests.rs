use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub(super) struct GateWrite<W> {
    pub(super) inner: W,
    pub(super) armed: Arc<AtomicBool>,
    pub(super) entered: Option<oneshot::Sender<()>>,
    pub(super) release: oneshot::Receiver<()>,
    pub(super) passed: bool,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for GateWrite<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.armed.load(Ordering::Acquire) && !self.passed {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            match Pin::new(&mut self.release).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(_) => self.passed = true,
            }
        }
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn bind(root: &Path, tool: bool, epoch: u32, corr: u64) -> Frame {
    let request = ModuleControlRequest::RouteBind {
        route_channel: 7,
        epoch,
        target: if tool {
            RouteTarget::ToolProvider {
                module_id: "aft".into(),
            }
        } else {
            RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            }
        },
        identity: subc_protocol::BindIdentity::new(root, "opencode", "retracted-bind"),
        principal: Some(subc_protocol::Principal::Direct),
        consumer_capabilities: None,
        admission_facts: Default::default(),
        scope: None,
        role_versions: None,
        call_mode: None,
    };
    Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap()
}

async fn retracted_bind(tool: bool, trigger: &str, expected_code: &str) {
    let (dir, root) = test_support::test_root("retracted-control-bind");
    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let storage = dir.path().join("storage");
    if tool {
        let ctx = Arc::new(crate::context::AppContext::from_app(
            app.clone(),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        ));
        assert!(executor.register_actor(root.clone(), ctx));
    }
    let user = dir.path().join("user.jsonc");
    std::fs::write(&user,r#"{"indexes":{"trigram":false,"semantic":false,"callgraph":false},"views":{"enabled":false}}"#).unwrap();
    let (mut daemon, module) = tokio::io::duplex(128 * 1024);
    let (read, write) = tokio::io::split(module);
    let armed = Arc::new(AtomicBool::new(false));
    let (entered, entering) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let frames = Arc::new(AtomicUsize::new(0));
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (events, mut observed) = mpsc::unbounded_channel();
    let probe = SubcTestLifecycleProbe::new(events).with_bind_ack_queue_for_test(891);
    let before = frames.clone();
    let hooks = stall_watchdog::FrameLoopTestHooks {
        metrics: metrics.clone(),
        config: None,
        before_frame: Arc::new(move || {
            before.fetch_add(1, Ordering::Release);
        }),
        before_turn_work: None,
        writer_setup: None,
    };
    let gated = GateWrite {
        inner: write,
        armed: armed.clone(),
        entered: Some(entered),
        release: released,
        passed: false,
    };
    let module = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(run_module_loop(
            read,
            gated,
            &storage.join("missing-connection.json"),
            app,
            executor,
            |request, ctx| crate::commands::configure::handle_configure(&request, ctx),
            Some(user),
            false,
            usize::MAX,
            Some(probe),
            &storage,
            QuiescedLsp::ReapInBackground,
            None,
            Some(hooks),
        ))
    });
    let hello = read_frame(&mut daemon).await.unwrap().unwrap();
    assert_eq!(hello.header.ty, FrameType::Hello);
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
    armed.store(true, Ordering::Release);
    write_frame(
        &mut daemon,
        &Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
    )
    .await
    .unwrap();
    entering.await.unwrap();
    write_frame(&mut daemon, &bind(root.as_path(), tool, 1, 891))
        .await
        .unwrap();
    let queued = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(SubcLifecycleEvent::RouteBindAckQueued { corr: 891, .. }) =
                observed.recv().await
            {
                break;
            }
        }
    })
    .await;
    if queued.is_ok() {
        let frame = match trigger {
            "goodbye" => {
                Frame::build(FrameType::Goodbye, control_flags(), 7, 1, 0, Vec::new()).unwrap()
            }
            "replacement" => bind(root.as_path(), tool, 2, 892),
            "stop" => {
                Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap()
            }
            _ => unreachable!(),
        };
        write_frame(&mut daemon, &frame).await.unwrap();
        if trigger != "stop" {
            write_frame(
                &mut daemon,
                &Frame::build(
                    FrameType::Request,
                    control_flags(),
                    0,
                    0,
                    999,
                    serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        }
    }
    let handled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if trigger == "stop" {
                if frames.load(Ordering::Acquire) >= 3 {
                    break;
                }
            } else if frames.load(Ordering::Acquire) >= 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let _ = release.send(());
    let mut terminals = Vec::new();
    let read_result = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(frame) = read_frame(&mut daemon).await.unwrap() {
            if frame.header.channel == 0 && frame.header.corr == 891 {
                terminals.push(frame);
            } else if trigger != "stop"
                && frame.header.ty == FrameType::Response
                && frame.header.corr == 999
            {
                break;
            }
        }
    })
    .await;
    drop(daemon);
    let result = module.join().unwrap();
    queued.unwrap_or_else(|error| {
        panic!(
            "ACK never queued: {error:?}; terminals={terminals:?}; loop={}; frames={}",
            metrics.frame_loop_context(),
            frames.load(Ordering::Acquire)
        )
    });
    handled.expect("retraction trigger never handled");
    read_result.expect("writer never drained");
    result.unwrap();
    assert_eq!(
        terminals.len(),
        1,
        "an open bind control request needs one terminal"
    );
    assert_eq!(terminals[0].header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&terminals[0].body).unwrap()["code"],
        expected_code
    );
    let expected_message = match trigger {
        "goodbye" => "request cancelled",
        "replacement" => "route bind superseded by a newer epoch",
        "stop" => "module is reloading",
        _ => unreachable!(),
    };
    assert_eq!(
        serde_json::from_slice::<Value>(&terminals[0].body).unwrap()["message"],
        expected_message
    );
}

#[tokio::test]
async fn retracted_bind_management_goodbye_has_one_control_error() {
    retracted_bind(false, "goodbye", "cancelled").await;
}
#[tokio::test]
async fn retracted_bind_management_replacement_has_one_control_error() {
    retracted_bind(false, "replacement", "bind_superseded").await;
}
#[tokio::test]
async fn retracted_bind_management_stop_has_one_control_error() {
    retracted_bind(false, "stop", "module_reloading").await;
}
#[tokio::test]
async fn retracted_bind_tool_goodbye_has_one_control_error() {
    retracted_bind(true, "goodbye", "cancelled").await;
}
#[tokio::test]
async fn retracted_bind_tool_replacement_has_one_control_error() {
    retracted_bind(true, "replacement", "bind_superseded").await;
}
#[tokio::test]
async fn retracted_bind_tool_stop_has_one_control_error() {
    retracted_bind(true, "stop", "module_reloading").await;
}
