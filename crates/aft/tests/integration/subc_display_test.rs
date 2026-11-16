#![cfg(unix)]

//! Exercise publication from the module's real route bind and tool-call entry.
use std::process::{Command, Stdio};
use std::time::Duration;

use cortexkit_role_tool_display::{validate_write, DisplayKind, DisplayLimits, DisplayWrite};
use serde_json::{json, Value};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::authenticate_server;
use tokio::net::{TcpListener, TcpStream};

use super::subc_detach_test::{
    control_flags, read_any_frame_timeout, send_frame, write_raw_connection_file,
    write_user_config, ModuleProcess,
};

const BASH_PIN: &str =
    "tp1:bash:019128a28f9f2724d2c086f93a1c74222b294ad95fee8b255fa799b257397a4f:2";

async fn respond(stream: &mut TcpStream, request: &Frame, value: Value) {
    send_frame(
        stream,
        Frame::build_with_version(
            request.header.ver,
            FrameType::Response,
            request.header.flags,
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

async fn request(stream: &mut TcpStream, channel: u16, corr: u64, value: Value) -> Frame {
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            if channel == 0 { 0 } else { 1 },
            corr,
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap(),
    )
    .await;
    loop {
        let frame = read_any_frame_timeout(stream, "module response").await;
        if frame.header.corr == corr
            && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
        {
            assert_eq!(
                frame.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&frame.body)
            );
            return frame;
        }
    }
}

async fn wait_for_ready_root(stream: &mut TcpStream, root: &std::path::Path) {
    let root = std::fs::canonicalize(root).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut corr = 1000;
    loop {
        let frame = request(
            stream,
            0,
            corr,
            serde_json::to_value(ModuleControlRequest::HealthCheck {}).unwrap(),
        )
        .await;
        corr += 1;
        let report: Value = serde_json::from_slice(&frame.body).unwrap();
        if report["metrics"]["stale"] != true
            && report["metrics"]["roots"].as_array().is_some_and(|roots| {
                roots
                    .iter()
                    .any(|entry| entry["project_root"] == json!(root) && entry["state"] == "ready")
            })
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "root never became ready: {report}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn fake_display(
    mut stream: TcpStream,
    mut frame: Frame,
    writes: tokio::sync::mpsc::UnboundedSender<DisplayWrite>,
) {
    let limits = DisplayLimits::new(16_384, 3_145_728, 16_384, 0);
    loop {
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        let reply = match body["op"].as_str().or(body["name"].as_str()) {
            Some("catalog.list") => json!({
                "op":"catalog.list", "generation":1,
                "modules":[{"module_id":"occipital", "ready":true, "roles":[], "control_ops":[], "capabilities": {
                    "provides":[cortexkit_role_tool_display::TOOL_DISPLAY_CAPABILITY],
                    "requires":[], "must_never_reach":[]
                }}], "subc_ops":["catalog.list","route.open"]
            }),
            Some("route.open") => {
                assert_eq!(
                    body["target"],
                    json!({"kind":"tool_provider","module_id":"occipital"})
                );
                assert_eq!(body["consumer_identity"]["module_id"], "aft");
                assert_eq!(
                    body["consumer_identity"]["launch_nonce"],
                    "display-test-nonce"
                );
                json!({"op":"route.open","route_channel":42,"route_epoch":1})
            }
            Some("role.describe") => json!({"tool_display":limits}),
            Some("display.write") => {
                // Decode the wire arguments with the same crate as the store,
                // not a permissive JSON object that could hide an invalid frame.
                let write: DisplayWrite =
                    serde_json::from_value(body["arguments"].clone()).unwrap();
                validate_write(&write, &limits).expect("store accepts display.write");
                writes.send(write).unwrap();
                json!({"result":"stored"})
            }
            Some("route.close") => json!({"op":"route.close"}),
            other => panic!("unexpected display request {other:?}: {body}"),
        };
        respond(&mut stream, &frame, reply).await;
        let Ok(Some(next)) = subc_transport::read_frame(&mut stream).await else {
            return;
        };
        frame = next;
    }
}

async fn run_display_case(switch: Option<&str>) {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let conn = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_user_config(config.path(), storage.path());
    let listener: TcpListener = write_raw_connection_file(conn.path()).await;
    let (modules_tx, mut modules) = tokio::sync::mpsc::unbounded_channel();
    let (writes_tx, mut writes) = tokio::sync::mpsc::unbounded_channel();
    let daemon = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let modules = modules_tx.clone();
                    let writes = writes_tx.clone();
                    connections.spawn(async move {
                        if authenticate_server(&mut stream, &[0x42; subc_transport::KEY_LEN],
                            &[0x24; subc_transport::DAEMON_ID_LEN], "subc-detach-test", Duration::from_secs(5)).await.is_err() {
                            return;
                        }
                        // Startup probes can authenticate and close without a
                        // frame. They are not module or display connections.
                        let Ok(Ok(Some(first))) = tokio::time::timeout(
                            Duration::from_secs(5), subc_transport::read_frame(&mut stream)
                        ).await else { return; };
                        if first.header.ty == FrameType::Hello {
                            modules.send((stream, first)).unwrap();
                        } else {
                            let body: Value = serde_json::from_slice(&first.body).unwrap();
                            if body["op"] == "catalog.list" || body["target"]["module_id"] == "occipital" {
                                fake_display(stream, first, writes).await;
                            } else {
                                crate::subc_fake::serve_without_modules(stream, first).await;
                            }
                        }
                    });
                }
                Some(result) = connections.join_next() => result.unwrap(),
            }
        }
    });
    let mut command = Command::new(crate::test_helpers::aft_binary::aft_binary());
    command
        .arg("--subc")
        .arg(conn.path().join("subc-connection.json"))
        .env("SUBC_MODULE_ID", "aft")
        .env("SUBC_LAUNCH_NONCE", "display-test-nonce")
        .env_remove("SUBC_LAUNCH_NONCE_FD")
        .env_remove("AFT_TOOL_DISPLAY")
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("XDG_CONFIG_HOME", config.path())
        .env("XDG_DATA_HOME", data.path())
        .env("AFT_STORAGE_DIR", storage.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(switch) = switch {
        command.env("AFT_TOOL_DISPLAY", switch);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let _module = ModuleProcess {
        child: command.spawn().unwrap(),
    };
    let (mut stream, hello) = tokio::time::timeout(Duration::from_secs(15), modules.recv())
        .await
        .unwrap()
        .unwrap();
    send_frame(
        &mut stream,
        Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: vec![],
                subc_capabilities: vec![],
                storage: None,
                machine_id: None,
            })
            .unwrap(),
        )
        .unwrap(),
    )
    .await;
    for (index, runner) in ["broca", "thalamus-harness"].into_iter().enumerate() {
        let channel = index as u16 + 1;
        let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new("opencode", format!("display-{runner}")),
            principal: Some(Principal::Reserved {
                module_id: runner.into(),
            }),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: Some([("tool-provider".into(), "v1".into())].into()),
            call_mode: None,
        })
        .unwrap();
        bind["identity"]["project_root"] = json!(project.path());
        request(&mut stream, 0, 10 + index as u64, bind).await;
        wait_for_ready_root(&mut stream, project.path()).await;
        let call_key = format!("display-lineage:{index}");
        let response = request(&mut stream, channel, 20 + index as u64, json!({
            "op":"tool.call", "name":"bash", "arguments":{"command":"printf display-ok","compressed":false},
            "call_key":call_key, "schema_pin":BASH_PIN,
        })).await;
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["isError"], false, "bash failed: {body}");
        if switch != Some("0") {
            let write = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let write = writes.recv().await.expect("fake display alive");
                    if write.kind == DisplayKind::Record && write.call_key == call_key {
                        break write;
                    }
                }
            })
            .await;
            let write = match write {
                Ok(write) => write,
                Err(_) => {
                    let report = request(
                        &mut stream,
                        0,
                        80 + index as u64,
                        serde_json::to_value(ModuleControlRequest::HealthCheck {}).unwrap(),
                    )
                    .await;
                    panic!(
                        "runner record never reached the display module: {}",
                        serde_json::from_slice::<Value>(&report.body).unwrap()["metrics"]
                            ["display"]
                    );
                }
            };
            assert_eq!(
                write.runner,
                Principal::Reserved {
                    module_id: runner.into()
                }
            );
            assert_eq!(write.schema_pin, BASH_PIN);
        }
    }
    let report = request(
        &mut stream,
        0,
        100,
        serde_json::to_value(ModuleControlRequest::HealthCheck {}).unwrap(),
    )
    .await;
    assert!(
        report.body.len() <= 12 * 1024,
        "health report exceeded 12 KiB"
    );
    let response: ModuleControlResponse = serde_json::from_slice(&report.body).unwrap();
    let report = response.health_report().unwrap();
    let metrics = report.metrics.as_ref().unwrap();
    let display = &metrics["display"];
    assert_eq!(display["enabled"], switch != Some("0"));
    if switch == Some("0") {
        assert_eq!(display["reason"], "opt_out");
        assert_eq!(display["counters"]["register_opt_out"], 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), writes.recv())
                .await
                .is_err(),
            "opted-out module published a record"
        );
    } else {
        assert!(display["counters"]["admitted"].as_u64().unwrap() >= 2);
        assert!(display["counters"]["stored"].as_u64().unwrap() >= 1);
    }
    daemon.abort();
    let _ = daemon.await;
}

#[tokio::test]
async fn display_default_module_delivers_real_runner_calls() {
    run_display_case(None).await;
}

#[tokio::test]
async fn display_explicit_opt_in_delivers_real_runner_calls() {
    run_display_case(Some("1")).await;
}

#[tokio::test]
async fn display_explicit_opt_out_counts_real_runner_calls() {
    run_display_case(Some("0")).await;
}
