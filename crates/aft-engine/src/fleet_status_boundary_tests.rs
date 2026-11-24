use aft_base::fleet_status::tests::RejectingConsumer;
use aft_base::fleet_status::*;
use std::{path::Path, sync::Arc};

#[tokio::test(start_paused = true)]
async fn status_kv_publishes_exact_segment_even_when_holder_refuses() {
    use crate::late_result_bus::status;
    let session = "status-kv:refused-holder/runner";
    status::route_bound(1, session);
    let (client, mut wire_rx) = FleetStatusClient::dial_channel(4);
    let text = "[AFT E? W1 | T2]";
    assert!(!client.publish(Path::new("/status-kv-project"), "opencode", session, text));
    let request = wire_rx.try_recv().unwrap();
    assert_eq!(request.session(), session);
    assert_eq!(
        aft_shell::test_bridge::bus::late_result_bus::status::put_session(session).await["segment"],
        request.body()["text"]
    );
    let identity = FleetRouteIdentity::from(&request);
    let consumer = RejectingConsumer {
        attempts: Arc::default(),
        advertises_status_line: true,
        catalog_calls: Arc::default(),
        publish_calls: Arc::default(),
        succeeds_on: None,
        failure: RouteOpenFailure::Refused {
            code: "capability_denied".into(),
            message: "refused".into(),
        },
        connection_callback: Arc::default(),
        push_sender: Arc::default(),
        control_sender: Arc::default(),
        watch: Arc::default(),
    };
    let task = tokio::spawn(run_connected_status_dial(
        consumer,
        client.clone(),
        wire_rx,
        identity,
        Some(request),
    ));
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(client.stopped_route().unwrap().session, session);
    let before = aft_shell::test_bridge::bus::late_result_bus::status::test_value(session).unwrap()
        ["generation"]
        .as_u64()
        .unwrap();
    client.publish(
        Path::new("/status-kv-project"),
        "opencode",
        session,
        "[AFT E0 W0 | T3]",
    );
    let value = aft_shell::test_bridge::bus::late_result_bus::status::put_session(session).await;
    assert_eq!(value["segment"], "[AFT E0 W0 | T3]");
    assert_eq!(value["generation"].as_u64().unwrap(), before + 1);
    status::route_unbound(1, session);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}
