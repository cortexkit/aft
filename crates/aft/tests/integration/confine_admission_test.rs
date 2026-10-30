//! A scoped worker's tool call needs its saved root confinement read from
//! aft.db before admission. That read must never hold the frame loop: while
//! another holder keeps the aft.db lock (a catalog job waiting for SQLite's
//! write lock can hold it for seconds), the module still answers health
//! checks, and the worker call is admitted once the lock comes free.

use super::*;

const WORKER_CHANNEL: u16 = 31;

async fn bind_scoped_worker(stream: &mut tokio::net::TcpStream, root: &std::path::Path) {
    let scope: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({
        "owner": {"kind": "direct"},
        "ref": "confine-admission-worker",
        "scope_epoch": 1,
        "kind": "worker",
        "owner_authorized": true,
    }))
    .unwrap();
    send_control_request(
        stream,
        WORKER_CHANNEL as u64,
        ModuleControlRequest::RouteBind {
            route_channel: WORKER_CHANNEL,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(root, "opencode", "confine-admission-worker"),
            principal: Some(Principal::Direct),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: Some(scope),
            role_versions: None,
            call_mode: None,
        },
    )
    .await;
    expect_route_bind_ack(stream, WORKER_CHANNEL as u64).await;
}

fn worker_read(root: &std::path::Path, corr: u64) -> Frame {
    Frame::build(
        FrameType::Request,
        control_flags(),
        WORKER_CHANNEL,
        1,
        corr,
        serde_json::to_vec(&json!({
            "name": "read",
            "preset": "worker",
            "arguments": {"path": root.join("admitted.txt")},
        }))
        .unwrap(),
    )
    .unwrap()
}

/// The bound root's context.
fn root_context(executor: &Arc<Executor>, root: &std::path::Path) -> Arc<AppContext> {
    let root = root.canonicalize().unwrap();
    executor
        .actor_contexts()
        .into_iter()
        .find(|ctx| {
            ctx.config()
                .project_root
                .as_deref()
                .and_then(|path| path.canonicalize().ok())
                .is_some_and(|path| path == root)
        })
        .unwrap_or_else(|| panic!("no context bound for {root:?}"))
}

#[test]
fn held_aft_db_lock_does_not_stop_the_frame_loop_while_worker_confinement_is_read() {
    run_subc_bridge_test(
        "held_aft_db_lock_does_not_stop_the_frame_loop_while_worker_confinement_is_read",
        Duration::from_secs(60),
        |input| async move {
            let FakeDaemonSession {
                mut stream,
                root1,
                executor,
                ..
            } = open_fake_daemon_session(input).await;
            std::fs::write(root1.join("admitted.txt"), "worker-read-admitted\n").unwrap();
            bind_scoped_worker(&mut stream, &root1).await;
            // The ACK precedes asynchronous database and scope registration.
            // A health round-trip lets the loop submit scope-bind; a mutating
            // lane barrier then waits for that job before we hold its database.
            let ctx = root_context(&executor, &root1);
            send_control_request(&mut stream, 30_001, ModuleControlRequest::HealthCheck {}).await;
            loop {
                let frame = read_frame_timeout(&mut stream, "configured root health").await;
                if frame.header.channel == 0 && frame.header.corr == 30_001 {
                    assert_eq!(frame.header.ty, FrameType::Response);
                    break;
                }
            }
            let ready = executor.submit_async(
                ProjectRootId::from_path(&root1).unwrap(),
                aft::executor::Lane::Mutating,
                "confine-fixture-ready".into(),
                Box::new(|_| Response::success("confine-fixture-ready", json!({}))),
            );
            assert!(
                tokio::time::timeout(Duration::from_secs(10), ready)
                    .await
                    .expect("scope registration did not settle")
                    .unwrap()
                    .success
            );
            let db = ctx
                .db()
                .expect("configured aft.db must be available after scope-bind");

            // Hold the aft.db lock on another thread, as a catalog job's
            // policy freeze does while SQLite waits for its write lock. The
            // holder lets go on its own after a bound, so a frame loop that
            // does block fails the timing check below instead of hanging.
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = thread::spawn(move || {
                let _conn = db.lock().unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            });
            held_rx.recv_timeout(Duration::from_secs(5)).unwrap();

            // This worker saved no confinement, so memory cannot answer for
            // it and its call needs an aft.db read before admission.
            send_frame(&mut stream, worker_read(&root1, 2)).await;
            send_control_request(&mut stream, 3, ModuleControlRequest::HealthCheck {}).await;
            let started = Instant::now();
            let health = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let frame =
                        read_frame_timeout(&mut stream, "health check while aft.db is held").await;
                    assert!(
                        !(frame.header.channel == WORKER_CHANNEL && frame.header.corr == 2),
                        "worker call answered before its confinement could be read: {}",
                        String::from_utf8_lossy(&frame.body)
                    );
                    if frame.header.channel == 0 && frame.header.corr == 3 {
                        return frame;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "health check unanswered after {:?} while aft.db was held",
                    started.elapsed()
                )
            });
            assert_eq!(health.header.ty, FrameType::Response);
            serde_json::from_slice::<ModuleControlResponse>(&health.body)
                .unwrap()
                .health_report()
                .expect("health report");

            release_tx.send(()).unwrap();
            holder.join().unwrap();
            let reply = loop {
                let frame = read_frame_timeout(&mut stream, "worker read after release").await;
                if frame.header.channel == WORKER_CHANNEL && frame.header.corr == 2 {
                    break frame;
                }
            };
            assert_eq!(reply.header.ty, FrameType::Response, "{reply:?}");
            assert!(
                !tool_result_is_error(&reply),
                "{}",
                tool_result_text(&reply)
            );
            assert!(
                tool_result_text(&reply).contains("worker-read-admitted"),
                "{}",
                tool_result_text(&reply)
            );
            send_connection_goodbye(&mut stream).await;
        },
        |_, _, _| {},
    );
}
