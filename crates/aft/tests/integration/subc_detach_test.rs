#![cfg(unix)]

// This regression drives the daemon's Unix-shaped drain restart: SIGTERM to the
// module process group. Windows uses CREATE_NEW_PROCESS_GROUP instead of POSIX
// process groups, so Windows coverage is the cross-target `cargo check` gate.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::ReleaseOnDrop;
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::{TcpListener, TcpStream};

const SESSION_ID: &str = "subc-detach-session";
const ROUTE_CHANNEL: u16 = 1;

#[test]
fn subc_background_bash_survives_module_process_group_restart() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    runtime.block_on(async {
        let project = tempfile::tempdir().expect("project tempdir");
        let storage = tempfile::tempdir().expect("storage tempdir");
        let conn_dir = tempfile::tempdir().expect("connection tempdir");
        let config_home = tempfile::tempdir().expect("config home tempdir");
        let data_home = tempfile::tempdir().expect("data home tempdir");
        write_user_config(config_home.path(), storage.path());

        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");

        let ready = project.path().join("bg.ready");
        let stop = project.path().join("bg.stop");
        // Declare after all TempDirs: Rust drops locals in reverse declaration
        // order, so this guard writes the sentinel before any TempDir removes its directory.
        let _stop_guard = ReleaseOnDrop::new(stop.clone());
        let command = sentinel_command(&ready, &stop);

        let mut first_module = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;

        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            20,
            "bash",
            json!({
                "command": command,
                "background": true,
                "timeout": 60_000,
                "compressed": false,
            }),
        )
        .await;
        let launch = read_tool_response(&mut stream, 20, "background bash launch").await;
        assert!(!tool_result_is_error(&launch), "launch failed: {}", frame_body(&launch));
        let task_id = extract_task_id(&launch);
        wait_for_path(&ready, "background task ready file");

        let running = bash_status(&mut stream, 21, &task_id).await;
        assert_eq!(running["status"], "running", "unexpected running status: {running}");
        let child_pid = running["child_pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .expect("running bash_status should report child_pid");

        let first_exit = first_module.terminate_process_group(libc::SIGTERM);
        assert_eq!(
            first_exit.code(),
            Some(128 + libc::SIGTERM),
            "module should exit through the subc signal handler after detaching; signal={:?}, status={first_exit}",
            first_exit.signal()
        );
        drop(stream);
        assert_process_alive(child_pid, "background child after module process-group SIGTERM");

        let mut second_module = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;

        let replayed = bash_status(&mut stream, 30, &task_id).await;
        assert_eq!(
            replayed["status"], "running",
            "fresh module should rehydrate running task: {replayed}"
        );
        assert_eq!(
            replayed["child_pid"].as_u64(),
            Some(u64::from(child_pid)),
            "rehydrated status should describe the same detached child"
        );
        assert_process_alive(child_pid, "rehydrated background child");

        drop(_stop_guard);
        let completed = wait_for_status(&mut stream, 31, &task_id, "completed").await;
        assert_eq!(completed["exit_code"], 0, "task should exit cleanly: {completed}");
        let output = completed["output_preview"].as_str().unwrap_or_default();
        assert!(
            output.contains("sentinel-stopped"),
            "completion should surface captured output, got {completed}"
        );

        send_connection_goodbye(&mut stream).await;
        let second_exit = second_module.wait_for_exit("second module graceful shutdown");
        assert!(second_exit.success(), "second module exit status: {second_exit}");
    });
}

fn write_user_config(config_home: &Path, storage: &Path) {
    let config_dir = config_home.join("cortexkit");
    std::fs::create_dir_all(&config_dir).expect("create user config dir");
    std::fs::write(
        config_dir.join("aft.jsonc"),
        serde_json::to_string(&json!({
            "storage_dir": storage,
            "bash": { "background": true },
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
        }))
        .expect("serialize user config"),
    )
    .expect("write user config");
}

async fn write_connection_file(conn_dir: &Path) -> TcpListener {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    std_listener
        .set_nonblocking(true)
        .expect("set fake daemon nonblocking");
    let port = std_listener.local_addr().expect("fake daemon addr").port();
    let conn_path = conn_dir.join("subc-connection.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_string(),
            port,
        }],
        key: vec![0x42; subc_transport::KEY_LEN],
        daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
        pid: std::process::id(),
        daemon_ver: "subc-detach-test".to_string(),
    };
    connection_file::write_atomic(&conn_path, &conn).expect("write connection file");
    TcpListener::from_std(std_listener).expect("tokio listener")
}

struct ModuleProcess {
    child: Child,
}

impl ModuleProcess {
    fn spawn(conn_path: &Path, config_home: &Path, data_home: &Path) -> Self {
        use std::os::unix::process::CommandExt;

        let binary = std::env::var_os("AFT_TEST_AFT_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")));
        let mut command = Command::new(binary);
        command
            .arg("--subc")
            .arg(conn_path)
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .env("XDG_CONFIG_HOME", config_home)
            .env("XDG_DATA_HOME", data_home)
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().expect("spawn aft --subc module"),
        }
    }

    fn pid_i32(&self) -> i32 {
        i32::try_from(self.child.id()).expect("module pid fits i32")
    }

    fn terminate_process_group(&mut self, signal: i32) -> ExitStatus {
        let rc = unsafe { libc::killpg(self.pid_i32(), signal) };
        assert_eq!(
            rc,
            0,
            "failed to signal module process group: {}",
            std::io::Error::last_os_error()
        );
        self.wait_for_exit("module process-group termination")
    }

    fn wait_for_exit(&mut self, label: &str) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Ok(None) => {
                    let _ = unsafe { libc::killpg(self.pid_i32(), libc::SIGKILL) };
                    let _ = self.child.wait();
                    panic!("timed out waiting for {label}");
                }
                Err(error) => panic!("wait for {label}: {error}"),
            }
        }
    }
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = unsafe { libc::killpg(self.pid_i32(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

async fn accept_module(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("timed out accepting module connection")
        .expect("accept module connection");
    authenticate_server(
        &mut stream,
        &[0x42; subc_transport::KEY_LEN],
        &[0x24; subc_transport::DAEMON_ID_LEN],
        "subc-detach-test",
        Duration::from_secs(5),
    )
    .await
    .expect("authenticate module");

    let hello = read_any_frame_timeout(&mut stream, "ModuleHello").await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    let _hello_body: ModuleHelloBody = serde_json::from_slice(&hello.body).expect("hello body");
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
                subc_ops: Vec::new(),
                subc_capabilities: Vec::new(),
                storage: None,
            })
            .expect("hello ack body"),
        )
        .expect("hello ack frame"),
    )
    .await;
    stream
}

async fn bind_route(stream: &mut TcpStream, root: &Path) {
    let project_cfg = root.join(".cortexkit").join("aft.jsonc");
    std::fs::create_dir_all(project_cfg.parent().expect("project config parent"))
        .expect("create project config dir");
    std::fs::write(
        &project_cfg,
        serde_json::to_string(&json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
        }))
        .expect("serialize project config"),
    )
    .expect("write project config");

    let request = ModuleControlRequest::RouteBind {
        route_channel: ROUTE_CHANNEL,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity {
            project_root: root.to_path_buf(),
            harness: "opencode".to_string(),
            session: SESSION_ID.to_string(),
        },
        principal: Some(Principal::Direct),
        consumer_capabilities: None,
        admission_facts: Default::default(),
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            10,
            serde_json::to_vec(&request).expect("route bind body"),
        )
        .expect("route bind frame"),
    )
    .await;

    let ack = read_frame_timeout(stream, "RouteBindAck").await;
    assert_eq!(ack.header.ty, FrameType::Response);
    assert_eq!(ack.header.channel, 0);
    assert_eq!(ack.header.corr, 10);
    let body: ModuleControlResponse = serde_json::from_slice(&ack.body).expect("ack body");
    assert_eq!(body, ModuleControlResponse::RouteBindAck {});
}

async fn send_tool_call(
    stream: &mut TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    arguments: Value,
) {
    let body = json!({ "name": name, "arguments": arguments });
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            1,
            corr,
            serde_json::to_vec(&body).expect("tool call body"),
        )
        .expect("tool call frame"),
    )
    .await;
}

async fn bash_status(stream: &mut TcpStream, corr: u64, task_id: &str) -> Value {
    send_tool_call(
        stream,
        ROUTE_CHANNEL,
        corr,
        "bash_status",
        json!({ "params": { "task_id": task_id } }),
    )
    .await;
    let frame = read_tool_response(stream, corr, "bash_status response").await;
    assert!(
        !tool_result_is_error(&frame),
        "bash_status failed: {}",
        frame_body(&frame)
    );
    tool_response_json(&frame)
}

async fn wait_for_status(
    stream: &mut TcpStream,
    start_corr: u64,
    task_id: &str,
    expected: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut corr = start_corr;
    loop {
        let status = bash_status(stream, corr, task_id).await;
        if status["status"] == expected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected}: {status}"
        );
        corr += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn send_connection_goodbye(stream: &mut TcpStream) {
    send_frame(
        stream,
        Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 99, Vec::new())
            .expect("goodbye frame"),
    )
    .await;
}

async fn send_frame(stream: &mut TcpStream, frame: Frame) {
    write_frame(stream, &frame).await.expect("write frame");
}

async fn read_any_frame_timeout(stream: &mut TcpStream, label: &str) -> Frame {
    tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .expect("read frame")
        .unwrap_or_else(|| panic!("EOF waiting for {label}"))
}

async fn read_frame_timeout(stream: &mut TcpStream, label: &str) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for {label}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for {label}"));
        if frame.header.ty != FrameType::Push {
            return frame;
        }
    }
}

async fn read_tool_response(stream: &mut TcpStream, corr: u64, label: &str) -> Frame {
    let frame = read_frame_timeout(stream, label).await;
    assert_eq!(frame.header.ty, FrameType::Response, "{label} frame type");
    assert_eq!(frame.header.channel, ROUTE_CHANNEL, "{label} channel");
    assert_eq!(frame.header.corr, corr, "{label} corr");
    frame
}

fn tool_response_json(frame: &Frame) -> Value {
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let structured = &body["structuredContent"];
    assert!(
        structured.is_object(),
        "tool response missing structuredContent envelope: {body}"
    );
    structured.clone()
}

fn tool_result_is_error(frame: &Frame) -> bool {
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    body["isError"].as_bool().unwrap_or(false)
}

fn frame_body(frame: &Frame) -> String {
    String::from_utf8_lossy(&frame.body).into_owned()
}

fn extract_task_id(frame: &Frame) -> String {
    let structured = tool_response_json(frame);
    if let Some(task_id) = structured.get("task_id").and_then(Value::as_str) {
        return task_id.to_string();
    }

    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let text = body["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    let start = text
        .find("bash-")
        .unwrap_or_else(|| panic!("no bash task id in {text:?}"));
    let tail = &text[start..];
    let end = tail
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .unwrap_or(tail.len());
    tail[..end].to_string()
}

fn sentinel_command(ready: &Path, stop: &Path) -> String {
    format!(
        "printf 'sentinel-started\\n'; touch {ready}; polls=0; while [ ! -f {stop} ] && [ \"$polls\" -lt 6000 ]; do sleep 0.05; polls=$((polls + 1)); done; if [ -f {stop} ]; then printf 'sentinel-stopped\\n'; else printf 'gate-timeout\\n'; fi",
        ready = shell_quote(ready),
        stop = shell_quote(stop),
    )
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_for_path(path: &Path, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn assert_process_alive(pid: u32, label: &str) {
    let pid = i32::try_from(pid).expect("pid fits i32");
    let alive = unsafe { libc::kill(pid, 0) == 0 }
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    assert!(alive, "{label} should still be alive (pid {pid})");
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}
