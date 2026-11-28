async fn accept_status_consumer(
    listener: &TcpListener,
    key: &[u8],
    daemon_id: &[u8; subc_transport::DAEMON_ID_LEN],
) -> (tokio::net::TcpStream, Frame) {
    loop {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("fleet consumer connection timeout")
            .expect("accept consumer");
        authenticate_server(
            &mut stream,
            key,
            daemon_id,
            FAKE_DAEMON_VERSION,
            Duration::from_secs(5),
        )
        .await
        .expect("authenticate consumer");
        let first = read_raw_inventory_frame(&mut stream, "consumer request").await;
        if crate::subc_fake::ckbus_route_refusal(&first).is_some()
            || frame_operation(&first).as_deref() == Some("supervisor.spawn_snapshot")
        {
            tokio::spawn(crate::subc_fake::serve_without_modules(stream, first));
            continue;
        }
        if frame_operation(&first).as_deref() == Some("route.open") {
            // The default-enabled GitHub shim also connects at startup. This
            // daemon has no vault; refuse its manifest route, not the fleet dial.
            let body: Value = serde_json::from_slice(&first.body).expect("manifest route body");
            assert_eq!(first.header.ty, FrameType::Request);
            assert_eq!(first.header.channel, 0);
            assert_eq!(
                body["target"],
                json!({ "kind": "management_surface", "module_id": "claustrum" })
            );
            assert_eq!(
                body["identity"],
                json!({ "harness": "aft-manifest-sync", "session": "daemon" })
            );
            send_channel_zero_error(&mut stream, first.header.corr, "target_unavailable").await;
            continue;
        }
        assert_eq!(frame_operation(&first).as_deref(), Some("catalog.list"));
        return (stream, first);
    }
}
