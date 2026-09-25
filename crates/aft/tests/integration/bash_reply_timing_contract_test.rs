#![cfg(unix)]

//! Cross-module contract: how long a `bash` tool call can hold its reply.
//!
//! Broca (the CortexKit agent runner) sizes how long it waits for an AFT tool
//! reply from these values:
//!   - `bash` with `wait: true` or `block_to_completion: true`:
//!     (timeout ?? 1,800,000 ms) + 30 s;
//!   - `bash_watch`: (timeoutMs ?? bash.watch_sync_max_ms) + 30 s;
//!   - any other tool: 120 s.
//!
//! Broca's waiting rule depends on every value pinned here. Changing any of
//! them (the 30-minute default hard timeout, the guarantee that a held call
//! answers once its timeout kills the command, or the bash_watch sync-wait
//! bounds) needs a coordinated change in Broca, or Broca will either give up on
//! a live call or wait on a dead one.
//!
//! The bash tests run the real `aft` binary twice over: once over the
//! stdin/stdout protocol and once as a `--subc` module behind a fake daemon,
//! because the subc path holds the call on its own deferred wait loop.
//!
//! `bash_watch` has no engine-side wait: the sync wait loop lives in the
//! OpenCode and Pi plugins, which read `bash.watch_sync_max_ms` from the
//! engine's resolved config. The last test pins that config value's default
//! and clamp range.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aft::bash_background::persistence::{read_task_at, resolve_task_layout, session_tasks_dir};
use aft::config::{
    DEFAULT_BASH_WATCH_SYNC_MAX_MS, MAX_BASH_WATCH_SYNC_MAX_MS, MIN_BASH_WATCH_SYNC_MAX_MS,
};
use aft::config_resolve::{resolve_config, ConfigTier};
use serde_json::{json, Value};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::{TcpListener, TcpStream};

use super::helpers::{user_config, AftProcess};

/// Hard timeout Broca assumes when a `bash` call omits `timeout`.
const CONSUMER_DEFAULT_HARD_TIMEOUT_MS: u64 = 1_800_000;
/// `bash.watch_sync_max_ms` default and ceiling Broca assumes.
const CONSUMER_WATCH_SYNC_DEFAULT_MS: u64 = 120_000;
const CONSUMER_WATCH_SYNC_MAX_MS: u64 = 1_800_000;

/// Short hard timeout for the timing tests.
const TIMEOUT_MS: u64 = 1_500;
/// How late after `TIMEOUT_MS` a held call may answer here. The engine needs
/// one watchdog tick (500 ms) to notice the expiry, a SIGTERM to the process
/// group, and one pending-response poll (100 ms). Three seconds is loose
/// enough for a loaded CI host and still far inside Broca's 30 s margin.
const REPLY_MARGIN: Duration = Duration::from_millis(3_000);
/// Backstop so a call that never answers fails the test instead of hanging it.
const HANG_CATCH: Duration = Duration::from_secs(15);

const SESSION_ID: &str = "bash-reply-timing-contract";
const ROUTE_CHANNEL: u16 = 1;

// ---------------------------------------------------------------------------
// (a) Omitted timeout resolves to 30 minutes in the engine.
// ---------------------------------------------------------------------------

#[test]
fn omitted_bash_timeout_resolves_to_thirty_minute_hard_cap() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path(), &storage);

    let response = aft.send(&bash_request(
        "omitted-timeout",
        json!({
            "command": "true",
            "foreground_orchestrate": true,
            "background": true,
        }),
    ));
    assert_eq!(response["success"], true, "response: {response:?}");
    let task_id = response["task_id"].as_str().expect("task_id").to_string();

    // The watchdog kills a task once it has run for `metadata.timeout_ms`, so
    // the persisted value is exactly the hard cap the engine enforces.
    assert_eq!(
        persisted_timeout_ms(&storage, &task_id),
        Some(CONSUMER_DEFAULT_HARD_TIMEOUT_MS),
        "engine default hard timeout changed; Broca's wait rule assumes 1,800,000 ms"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn subc_omitted_bash_timeout_resolves_to_thirty_minute_hard_cap() {
    run_subc(|mut harness| async move {
        send_tool_call(
            &mut harness.stream,
            20,
            "bash",
            json!({ "command": "true", "background": true, "compressed": false }),
        )
        .await;
        let launch = read_tool_response(&mut harness.stream, 20, HANG_CATCH).await;
        assert!(
            !tool_result_is_error(&launch),
            "launch: {}",
            frame_body(&launch)
        );
        let task_id = extract_task_id(&launch);
        assert_eq!(
            persisted_timeout_ms(&harness.storage_root(), &task_id),
            Some(CONSUMER_DEFAULT_HARD_TIMEOUT_MS),
            "subc translation or engine default changed the omitted-timeout hard cap"
        );
        harness
    });
}

// ---------------------------------------------------------------------------
// (b)+(c) A held call answers terminal once its timeout kills the command.
// ---------------------------------------------------------------------------

#[test]
fn wait_true_answers_timed_out_within_timeout_plus_margin() {
    assert_standalone_held_call_times_out("wait");
}

#[test]
fn block_to_completion_answers_timed_out_within_timeout_plus_margin() {
    assert_standalone_held_call_times_out("block_to_completion");
}

#[test]
fn subc_wait_true_answers_timed_out_within_timeout_plus_margin() {
    assert_subc_held_call_times_out("wait");
}

#[test]
fn subc_block_to_completion_answers_timed_out_within_timeout_plus_margin() {
    assert_subc_held_call_times_out("block_to_completion");
}

fn assert_standalone_held_call_times_out(flag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let pidfile = dir.path().join("grandchild.pid");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path(), &storage);

    let mut params = json!({
        "command": sleeping_command(&pidfile),
        "foreground_orchestrate": true,
        "timeout": TIMEOUT_MS,
        "compressed": false,
    });
    params[flag] = json!(true);

    let started = Instant::now();
    let response = aft.send_with_timeout(&bash_request("held-timeout", params), HANG_CATCH);
    let elapsed = started.elapsed();

    assert_eq!(response["success"], true, "response: {response:?}");
    assert_eq!(
        response["status"], "timed_out",
        "{flag}: response: {response:?}"
    );
    assert_eq!(
        response["timed_out"], true,
        "{flag}: response: {response:?}"
    );
    assert_reply_window(flag, elapsed);
    assert_grandchild_killed(&pidfile);
    assert!(aft.shutdown().success());
}

fn assert_subc_held_call_times_out(flag: &'static str) {
    run_subc(move |mut harness| async move {
        let pidfile = harness.project.path().join("grandchild.pid");
        let mut arguments = json!({
            "command": sleeping_command(&pidfile),
            "timeout": TIMEOUT_MS,
            "compressed": false,
        });
        arguments[flag] = json!(true);

        let started = Instant::now();
        send_tool_call(&mut harness.stream, 30, "bash", arguments).await;
        let frame = read_tool_response(&mut harness.stream, 30, HANG_CATCH).await;
        let elapsed = started.elapsed();

        let structured = tool_response_json(&frame);
        assert_eq!(
            structured["status"],
            "timed_out",
            "subc {flag}: reply was not terminal: {}",
            frame_body(&frame)
        );
        assert_reply_window(flag, elapsed);
        assert_grandchild_killed(&pidfile);
        harness
    });
}

/// A shell that starts a `sleep` grandchild in the shell's own process group,
/// records the grandchild's pid, and waits on it. Killing only the shell would
/// leave the grandchild alive, so its death proves the whole group was killed.
fn sleeping_command(pidfile: &Path) -> String {
    format!("sleep 30 & echo $! > {}; wait", shell_quote(pidfile))
}

fn assert_reply_window(flag: &str, elapsed: Duration) {
    let timeout = Duration::from_millis(TIMEOUT_MS);
    assert!(
        elapsed >= timeout,
        "{flag}: answered after {elapsed:?}, before its {timeout:?} timeout could fire"
    );
    assert!(
        elapsed <= timeout + REPLY_MARGIN,
        "{flag}: answered after {elapsed:?}, later than timeout {timeout:?} + margin {REPLY_MARGIN:?}"
    );
}

fn assert_grandchild_killed(pidfile: &Path) {
    let pid: i32 = std::fs::read_to_string(pidfile)
        .expect("grandchild pid file")
        .trim()
        .parse()
        .expect("grandchild pid");
    // SIGKILL is sent before the reply, but the orphaned grandchild may linger
    // briefly as a zombie until init reaps it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "grandchild {pid} survived the timeout kill; the process group was not killed"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// (d) bash.watch_sync_max_ms: default 120,000, clamped to 1,000..=1,800,000.
// ---------------------------------------------------------------------------

#[test]
fn bash_watch_sync_max_default_and_clamp_match_consumer_contract() {
    assert_eq!(
        DEFAULT_BASH_WATCH_SYNC_MAX_MS,
        CONSUMER_WATCH_SYNC_DEFAULT_MS
    );
    assert_eq!(MAX_BASH_WATCH_SYNC_MAX_MS, CONSUMER_WATCH_SYNC_MAX_MS);
    assert_eq!(MIN_BASH_WATCH_SYNC_MAX_MS, 1_000);

    let resolved = |doc: &str| {
        let tiers = if doc.is_empty() {
            Vec::new()
        } else {
            vec![ConfigTier {
                tier: "user".to_string(),
                source: "bash_reply_timing_contract".to_string(),
                doc: doc.to_string(),
            }]
        };
        resolve_config(&tiers).config.bash.watch_sync_max_ms
    };

    assert_eq!(resolved(""), CONSUMER_WATCH_SYNC_DEFAULT_MS, "omitted");
    assert_eq!(
        resolved(r#"{ "bash": { "watch_sync_max_ms": 600000 } }"#),
        600_000,
        "in range"
    );
    assert_eq!(
        resolved(r#"{ "bash": { "watch_sync_max_ms": 99999999 } }"#),
        CONSUMER_WATCH_SYNC_MAX_MS,
        "above the ceiling must clamp to 1,800,000"
    );
    assert_eq!(
        resolved(r#"{ "bash": { "watch_sync_max_ms": 5 } }"#),
        1_000,
        "below the floor must clamp to 1,000"
    );
}

// ---------------------------------------------------------------------------
// Standalone (stdin/stdout) helpers.
// ---------------------------------------------------------------------------

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path) {
    let response = aft.send(
        &json!({
            "id": "cfg-bash-reply-timing",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": user_config(json!({ "bash": { "background": true } })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn bash_request(id: &str, params: Value) -> String {
    json!({
        "id": id,
        "method": "bash",
        "session_id": SESSION_ID,
        "params": params,
    })
    .to_string()
}

fn persisted_timeout_ms(storage: &Path, task_id: &str) -> Option<u64> {
    let layout = resolve_task_layout(&session_tasks_dir(storage, SESSION_ID), task_id)
        .unwrap_or_else(|error| panic!("resolve persisted task {task_id}: {error}"));
    read_task_at(&layout)
        .unwrap_or_else(|error| panic!("read persisted task {task_id}: {error}"))
        .timeout_ms
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Subc helpers: a fake daemon that accepts one `aft --subc` module.
// ---------------------------------------------------------------------------

struct SubcHarness {
    stream: TcpStream,
    project: tempfile::TempDir,
    data_home: tempfile::TempDir,
}

impl SubcHarness {
    /// A subc module keeps its state under `$XDG_DATA_HOME/cortexkit/aft`.
    fn storage_root(&self) -> PathBuf {
        self.data_home.path().join("cortexkit").join("aft")
    }
}

/// Runs `body` against a freshly bound subc module, then shuts it down. The
/// body hands the harness back so the temp dirs outlive the module.
fn run_subc<F, Fut>(body: F)
where
    F: FnOnce(SubcHarness) -> Fut,
    Fut: std::future::Future<Output = SubcHarness>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let project = tempfile::tempdir().expect("project tempdir");
        let conn_dir = tempfile::tempdir().expect("connection tempdir");
        let config_home = tempfile::tempdir().expect("config home tempdir");
        let data_home = tempfile::tempdir().expect("data home tempdir");
        write_user_config(config_home.path());

        let listener = write_connection_file(conn_dir.path()).await;
        let mut module = ModuleProcess::spawn(
            &conn_dir.path().join("subc-connection.json"),
            config_home.path(),
            data_home.path(),
        );
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;

        let mut harness = body(SubcHarness {
            stream,
            project,
            data_home,
        })
        .await;

        send_frame(
            &mut harness.stream,
            Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 99, Vec::new())
                .expect("goodbye frame"),
        )
        .await;
        module.wait_for_exit();
    });
}

fn write_user_config(config_home: &Path) {
    let config_dir = config_home.join("cortexkit");
    std::fs::create_dir_all(&config_dir).expect("create user config dir");
    std::fs::write(
        config_dir.join("aft.jsonc"),
        serde_json::to_string(&json!({
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
        daemon_ver: "bash-reply-timing-contract".to_string(),
    };
    connection_file::write_atomic(&conn_dir.join("subc-connection.json"), &conn)
        .expect("write connection file");
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
        // Own process group, so Drop can kill the module and any bash children.
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

    fn pgid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("module pid fits i32")
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Ok(None) => panic!("subc module did not exit after Goodbye"),
                Err(error) => panic!("wait for subc module: {error}"),
            }
        }
    }
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = unsafe { libc::killpg(self.pgid(), libc::SIGKILL) };
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
        "bash-reply-timing-contract",
        Duration::from_secs(5),
    )
    .await
    .expect("authenticate module");

    let hello = read_non_push_frame(&mut stream, Duration::from_secs(30), "ModuleHello").await;
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
        identity: BindIdentity::new(
            root.to_path_buf(),
            "opencode".to_string(),
            SESSION_ID.to_string(),
        ),
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

    let ack = read_non_push_frame(stream, Duration::from_secs(30), "RouteBindAck").await;
    assert_eq!(ack.header.ty, FrameType::Response);
    let body: ModuleControlResponse = serde_json::from_slice(&ack.body).expect("ack body");
    assert_eq!(body, ModuleControlResponse::RouteBindAck {});
}

async fn send_tool_call(stream: &mut TcpStream, corr: u64, name: &str, arguments: Value) {
    let body = json!({ "name": name, "arguments": arguments });
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            ROUTE_CHANNEL,
            1,
            corr,
            serde_json::to_vec(&body).expect("tool call body"),
        )
        .expect("tool call frame"),
    )
    .await;
}

async fn send_frame(stream: &mut TcpStream, frame: Frame) {
    write_frame(stream, &frame).await.expect("write frame");
}

/// Reads frames until a non-push frame arrives, failing if none arrives
/// within `limit` of the call.
async fn read_non_push_frame(stream: &mut TcpStream, limit: Duration, label: &str) -> Frame {
    let deadline = Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "no {label} within {limit:?}");
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("no {label} within {limit:?}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for {label}"));
        if frame.header.ty != FrameType::Push {
            return frame;
        }
    }
}

async fn read_tool_response(stream: &mut TcpStream, corr: u64, limit: Duration) -> Frame {
    let frame = read_non_push_frame(stream, limit, "tool response").await;
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "tool response frame type"
    );
    assert_eq!(frame.header.channel, ROUTE_CHANNEL, "tool response channel");
    assert_eq!(frame.header.corr, corr, "tool response corr");
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
    if let Some(task_id) = tool_response_json(frame)
        .get("task_id")
        .and_then(Value::as_str)
    {
        return task_id.to_string();
    }
    // Some launch replies carry the id only in the rendered text.
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let text = body["content"][0]["text"].as_str().unwrap_or_default();
    let start = text
        .find("bash-")
        .unwrap_or_else(|| panic!("no task_id in tool response: {}", frame_body(frame)));
    let tail = &text[start..];
    let end = tail
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .unwrap_or(tail.len());
    tail[..end].to_string()
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}
