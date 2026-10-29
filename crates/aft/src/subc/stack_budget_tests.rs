use super::*;

#[test]
fn runtime_stack_worker_joins_and_preserves_results() {
    let caller = std::thread::current().id();
    let result = with_module_runtime_stack(|| {
        assert_ne!(std::thread::current().id(), caller);
        assert_eq!(std::thread::current().name(), Some("aft-subc-runtime"));
        Err::<(), _>(SubcError::ConnectionLost)
    })
    .unwrap();
    assert!(matches!(result, Err(SubcError::ConnectionLost)));
    assert!(with_module_runtime_stack(|| Ok::<(), SubcError>(()))
        .unwrap()
        .is_ok());
}

#[test]
fn runtime_stack_worker_resumes_original_panic() {
    let payload = Arc::new(());
    let worker_payload = Arc::clone(&payload);
    let panic = std::panic::catch_unwind(|| {
        let _ = with_module_runtime_stack(move || std::panic::panic_any(worker_payload));
    })
    .expect_err("the caller must unwind when the runtime thread panics");
    assert!(Arc::ptr_eq(&payload, &panic.downcast::<Arc<()>>().unwrap()));
}

#[tokio::test(flavor = "current_thread")]
async fn module_loop_control_request_fits_with_stack_headroom() {
    // Exercise the handshake, health-check, and goodbye poll path on a quarter
    // of the production runtime's stack. Future size alone cannot measure poll
    // frames, and other request paths still need their own runtime coverage.
    const GUARD_STACK_BYTES: usize = 4 * 1024 * 1024;
    assert!(MODULE_RUNTIME_STACK_BYTES >= 4 * GUARD_STACK_BYTES);
    let storage = tempfile::tempdir().unwrap();
    let storage_path = storage.path().to_path_buf();
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let worker = std::thread::Builder::new()
        .name("subc-stack-guard".to_string())
        .stack_size(GUARD_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (read, write) = tokio::io::split(module);
            let connection_file = storage_path.join("absent-connection.json");
            runtime.block_on(heap_module_future(|| {
                run_module_loop(
                    read,
                    write,
                    &connection_file,
                    App::default_shared(),
                    Arc::new(Executor::new()),
                    |_, _| unreachable!("a health request needs no executor job"),
                    None,
                    false,
                    MAX_FRAME_BODY_LEN as usize,
                    None,
                    &storage_path,
                    QuiescedLsp::ReapInBackground,
                    None,
                    None,
                )
            }))
        })
        .unwrap();

    let hello = receive(&mut daemon).await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    send(&mut daemon, FrameType::HelloAck, HELLO_CORR, b"{}".to_vec()).await;
    let corr = HELLO_CORR + 100;
    send(
        &mut daemon,
        FrameType::Request,
        corr,
        serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).unwrap(),
    )
    .await;
    let reply = receive(&mut daemon).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    assert_eq!(reply.header.channel, 0);
    assert_eq!(reply.header.corr, corr);
    assert!(serde_json::from_slice::<ModuleControlResponse>(&reply.body)
        .unwrap()
        .health_report()
        .is_some());

    send(&mut daemon, FrameType::Goodbye, corr + 1, Vec::new()).await;
    // Closing the daemon half lets the module's teardown observe EOF instead
    // of spending its exit budget waiting for a peer that has already stopped.
    drop(daemon);
    assert_eq!(worker.join().unwrap().unwrap(), ModuleLoopExit::Graceful);
}

async fn send(daemon: &mut tokio::io::DuplexStream, ty: FrameType, corr: u64, body: Vec<u8>) {
    let frame = Frame::build(ty, control_flags(), 0, 0, corr, body).unwrap();
    write_frame(daemon, &frame).await.unwrap();
}

async fn receive(daemon: &mut tokio::io::DuplexStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(20), read_frame(daemon))
        .await
        .expect("module loop must answer within the request budget")
        .unwrap()
        .unwrap()
}
