//! Isolated broker for the views performance harness. Only production subc
//! frames cross into the AFT child; no indexing or readiness internals are used.
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use subc_protocol::session::ModuleControlRequest;
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::net::{TcpListener, TcpStream};

// Blocking stdin on a current-thread runtime also blocks the socket reader.
// Keep exactly one async reader and writer, and correlate every reply by ID.
async fn pipeline(stream: TcpStream) {
    let (mut reader, mut writer) = stream.into_split();
    let mut replies = tokio::spawn(async move {
        while let Some(frame) = read_frame(&mut reader).await.expect("broker frame") {
            if matches!(frame.header.ty, FrameType::Response | FrameType::Error) {
                let body: Value = serde_json::from_slice(&frame.body).expect("reply JSON");
                emit(json!({"id":frame.header.corr.to_string(), "body":body,
                    "transport_error":frame.header.ty == FrameType::Error}));
            }
        }
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if tx.blocking_send(line.expect("stdin line")).is_err() {
                break;
            }
        }
    });
    loop {
        let line = tokio::select! {
            line = rx.recv() => match line { Some(line) => line, None => break },
            _ = &mut replies => panic!("subject disconnected"),
        };
        let command: Value = serde_json::from_str(&line).expect("command JSON");
        let corr = command["id"]
            .as_str()
            .expect("string request ID")
            .parse()
            .expect("numeric ID");
        let channel = command["channel"].as_u64().unwrap_or(0) as u16;
        let body = match command["op"].as_str().expect("operation") {
            "bind" => serde_json::to_value({
                let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
                    route_channel: channel,
                    epoch: 1,
                    target: RouteTarget::ToolProvider {
                        module_id: "aft".into(),
                    },
                    identity: BindIdentity::new("opencode", format!("views-perf-{channel}")),
                    consumer_capabilities: None,
                    principal: Some(Principal::Direct),
                    admission_facts: Default::default(),
                    scope: None,
                    role_versions: None,
                    call_mode: None,
                })
                .unwrap();
                bind["identity"]["project_root"] =
                    serde_json::json!(PathBuf::from(command["root"].as_str().unwrap()));
                if bind["scope"].is_object() && bind["scope"].get("workdir").is_none() {
                    bind["scope"]["workdir"] =
                        serde_json::json!(PathBuf::from(command["root"].as_str().unwrap()));
                }
                bind
            })
            .unwrap(),
            "tool" => json!({"name":command["name"], "arguments":command["arguments"]}),
            "health" => serde_json::to_value(ModuleControlRequest::HealthCheck {}).unwrap(),
            other => panic!("unsupported pipelined operation {other}"),
        };
        let route = if command["op"] == "tool" { channel } else { 0 };
        write_frame(
            &mut writer,
            &Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                route,
                u32::from(route != 0),
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    }
    replies.abort();
}

struct Subject(Child);
impl Drop for Subject {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn exchange(stream: &mut TcpStream, channel: u16, corr: u64, body: Value) -> Value {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        u32::from(channel != 0),
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let frame = read_frame(stream)
                .await
                .unwrap()
                .expect("subject disconnected");
            if frame.header.corr == corr
                && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
            {
                let response: Value = serde_json::from_slice(&frame.body).unwrap();
                assert_ne!(frame.header.ty, FrameType::Error, "{response}");
                return response;
            }
        }
    })
    .await
    .expect("public request deadline")
}

fn emit(value: Value) {
    println!("{value}");
    io::stdout().flush().unwrap();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert!(
        args.len() == 4 || (args.len() == 5 && args[4] == "--pipelined"),
        "usage: views_perf_daemon AFT_BINARY RUN_DIR INITIAL_ROOT [--pipelined]"
    );
    let binary = fs::canonicalize(&args[1]).unwrap();
    let out = fs::canonicalize(&args[2]).unwrap();
    let root = fs::canonicalize(&args[3]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let conn = out.join("connection.json");
    connection_file::write_atomic(
        &conn,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: key.clone(),
            daemon_id,
            pid: std::process::id(),
            daemon_ver: "views-performance-broker-v1".into(),
        },
    )
    .unwrap();
    let log = fs::File::create(out.join("daemon.log")).unwrap();
    let mut child = Subject(
        Command::new(binary)
            .arg("--subc")
            .arg(&conn)
            .env("AFT_STORAGE_DIR", out.join("storage"))
            .env("HOME", out.join("home"))
            .env("XDG_CONFIG_HOME", out.join("config"))
            .env("XDG_DATA_HOME", out.join("data"))
            .env("XDG_CACHE_HOME", out.join("cache"))
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(60), listener.accept())
        .await
        .unwrap()
        .unwrap();
    authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        "views-performance-broker-v1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let hello = read_frame(&mut stream).await.unwrap().unwrap();
    assert_eq!(hello.header.ty, FrameType::Hello);
    write_frame(
        &mut stream,
        &Frame::build(
            FrameType::HelloAck,
            Flags::new(false, Priority::Passive, false),
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
    .await
    .unwrap();
    emit(
        json!({"pid":child.0.id(), "registry":out.join("storage"), "wire_version":PROTOCOL_VERSION, "hello":serde_json::from_slice::<Value>(&hello.body).unwrap()}),
    );
    if args.len() == 5 {
        pipeline(stream).await;
        return;
    }
    let stdin = io::stdin();
    let mut corr = 100;
    for line in stdin.lock().lines() {
        let command: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if command["op"] == "batch" {
            let mut pending = HashMap::new();
            for request in command["requests"].as_array().expect("batch requests") {
                corr += 1;
                let channel = request["channel"].as_u64().unwrap_or(1) as u16;
                let body = json!({"name":request["name"], "arguments":request["arguments"]});
                pending.insert(corr, (request["id"].clone(), Instant::now()));
                write_frame(
                    &mut stream,
                    &Frame::build(
                        FrameType::Request,
                        Flags::new(false, Priority::Interactive, false),
                        channel,
                        1,
                        corr,
                        serde_json::to_vec(&body).unwrap(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(600), async {
                while !pending.is_empty() {
                    let frame = read_frame(&mut stream).await.unwrap().expect("subject disconnected");
                    if matches!(frame.header.ty, FrameType::Response | FrameType::Error) {
                        if let Some((id, started)) = pending.remove(&frame.header.corr) {
                            let response: Value = serde_json::from_slice(&frame.body).unwrap();
                            assert_ne!(frame.header.ty, FrameType::Error, "{response}");
                            emit(json!({"id":id, "latency_ms":started.elapsed().as_secs_f64() * 1000.0, "response":response}));
                        }
                    }
                }
            }).await.expect("parallel public request deadline");
            continue;
        }
        corr += 1;
        let channel = command["channel"].as_u64().unwrap_or(0) as u16;
        let body = match command["op"].as_str().unwrap() {
            "bind" => serde_json::to_value({
                let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
                    route_channel: channel,
                    epoch: 1,
                    target: RouteTarget::ToolProvider {
                        module_id: "aft".into(),
                    },
                    identity: BindIdentity::new("opencode", format!("views-perf-{channel}")),
                    consumer_capabilities: None,
                    principal: Some(Principal::Direct),
                    admission_facts: Default::default(),
                    scope: None,
                    role_versions: None,
                    call_mode: None,
                })
                .unwrap();
                bind["identity"]["project_root"] =
                    serde_json::json!(PathBuf::from(command["root"].as_str().unwrap()));
                if bind["scope"].is_object() && bind["scope"].get("workdir").is_none() {
                    bind["scope"]["workdir"] =
                        serde_json::json!(PathBuf::from(command["root"].as_str().unwrap()));
                }
                bind
            })
            .unwrap(),
            "health" => serde_json::to_value(ModuleControlRequest::HealthCheck {}).unwrap(),
            "tool" => json!({"name":command["name"],"arguments":command["arguments"]}),
            "unbind" => {
                write_frame(
                    &mut stream,
                    &Frame::build(
                        FrameType::Goodbye,
                        Flags::new(false, Priority::Passive, false),
                        channel,
                        1,
                        0,
                        vec![],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
                emit(json!({"unbound":channel}));
                continue;
            }
            "quit" => break,
            other => panic!("unsupported broker operation {other}"),
        };
        let request_channel = if command["op"] == "tool" { channel } else { 0 };
        emit(exchange(&mut stream, request_channel, corr, body).await);
    }
    let _ = child.0.kill();
}
