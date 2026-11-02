use std::time::Duration;

use aft::subc::SubcLifecycleEvent;
use serde_json::json;

use super::subc_bridge_test::{self, FakeDaemonInput};
use super::subc_storm_test::{
    expect_ack_or_watchdog, expect_lifecycle_event, expect_tool_response_or_watchdog, send_bind,
    send_goodbye_and_wait, send_route_goodbye, send_tool, start_io, storm_dispatch,
    storm_project_config,
};

#[test]
fn tool_output_cap_read_after_last_route_close_and_rebind() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch_and_lifecycle_probe(
        "tool_output_cap_read_after_last_route_close_and_rebind",
        Duration::from_secs(45),
        drive_tool_output_cap_rebind_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

async fn drive_tool_output_cap_rebind_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    let mut lifecycle_events = session
        .lifecycle_events
        .expect("route lifecycle probe installed");
    let (tx, mut rx) = start_io(session.stream);
    const SESSION: &str = "output-cap-rebind-session";
    send_bind(
        &tx,
        1,
        4000,
        &root,
        SESSION,
        storm_project_config(false, false, false, 0),
    );
    expect_ack_or_watchdog(&mut rx, 4000).await;
    send_tool(
        &tx,
        1,
        4001,
        "subc_storm_egress_payload",
        json!({"payload_bytes":65_536}),
    );
    let cut = expect_tool_response_or_watchdog(&mut rx, 4001).await;
    let text = cut["text"].as_str().expect("rendered cut text");
    assert!(text.len() <= 51_200);
    let path = text
        .rsplit_once("; full result saved to ")
        .unwrap()
        .1
        .strip_suffix("; read it with read and a line range]")
        .unwrap()
        .to_owned();
    send_route_goodbye(&tx, 1, 4002);
    expect_lifecycle_event(
        &mut lifecycle_events,
        SubcLifecycleEvent::RouteDetached {
            route_channel: 1,
            route_epoch: 1,
            session_id: SESSION.into(),
        },
    )
    .await;
    send_bind(
        &tx,
        2,
        4003,
        &root,
        SESSION,
        storm_project_config(false, false, false, 0),
    );
    expect_ack_or_watchdog(&mut rx, 4003).await;
    send_tool(
        &tx,
        2,
        4004,
        "read",
        json!({"path":path,"startLine":1,"endLine":1}),
    );
    let read = expect_tool_response_or_watchdog(&mut rx, 4004).await;
    assert!(read["byte_size"].as_u64().unwrap() > 51_200, "{read:?}");
    assert_eq!(read["lines_read"], 1);
    assert!(read["content"].as_str().unwrap().contains("payload"));
    send_goodbye_and_wait(&tx).await;
}
