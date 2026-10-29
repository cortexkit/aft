//! Real daemon frames and real shell processes exercise scope lifetime.

use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
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
use tokio::{net::TcpListener, sync::mpsc};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

struct Daemon {
    root: PathBuf,
    child: Child,
    writer: tokio::net::tcp::OwnedWriteHalf,
    frames: mpsc::UnboundedReceiver<Frame>,
    reader: tokio::task::JoinHandle<()>,
    parked: VecDeque<Frame>,
    corr: u64,
    controls: Vec<(u64, String)>,
    stderr_path: PathBuf,
    frame_trace: Vec<String>,
    #[cfg(feature = "test-timing-hooks")]
    describe_clock: tempfile::TempDir,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.reader.abort();
        let _ = self.child.kill();
        if let Err(reason) = wait_for_child_exit(&mut self.child, Duration::from_secs(2)) {
            if std::thread::panicking() {
                eprintln!("scope daemon cleanup failed: {reason}");
            } else {
                panic!("scope daemon cleanup failed: {reason}");
            }
        }
        if std::thread::panicking() {
            // Assertions outside reply() also need the control-frame history.
            eprintln!(
                "scope daemon sent/received frames:\n{}",
                self.frame_trace.join("\n")
            );
            while let Ok(frame) = self.frames.try_recv() {
                eprintln!(
                    "scope daemon unread frame at failure: type={:?} channel={} epoch={} corr={} body={}",
                    frame.header.ty,
                    frame.header.channel,
                    frame.header.epoch,
                    frame.header.corr,
                    String::from_utf8_lossy(&frame.body)
                );
            }
        }
        if std::thread::panicking() || std::env::var_os("AFT_TEST_SCOPE_DIAGNOSTICS").is_some() {
            eprintln!(
                "scope daemon stderr:\n{}",
                std::fs::read_to_string(&self.stderr_path)
                    .unwrap_or_else(|error| error.to_string())
            );
        }
    }
}
fn wait_for_child_exit(child: &mut Child, within: Duration) -> Result<(), String> {
    let until = std::time::Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Err(error) => return Err(format!("cannot reap daemon: {error}")),
            Ok(None) => {}
        }
        if std::time::Instant::now() >= until {
            return Err(format!("daemon did not exit within {within:?} after kill"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn daemon_cleanup_bounds_wait_even_if_kill_does_not_stop_the_child() {
    #[cfg(unix)]
    let mut child = Command::new("sh").args(["-c", "sleep 1"]).spawn().unwrap();
    #[cfg(windows)]
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 1"])
        .spawn()
        .unwrap();
    let result = wait_for_child_exit(&mut child, Duration::from_millis(30));
    let _ = child.kill();
    wait_for_child_exit(&mut child, Duration::from_secs(2)).unwrap();
    assert!(result
        .unwrap_err()
        .contains("daemon did not exit within 30ms after kill"));
}
fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}
fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap_or(Value::Null)
}
fn stamp(owner: &str, epoch: u64) -> subc_protocol::scope::ScopeStamp {
    subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: owner.into(),
        },
        "same-ref",
        epoch,
        subc_protocol::scope::ScopeKind::Head,
    )
    .with_owner_authorized(true)
}
fn sleep_command(seconds: u64) -> String {
    #[cfg(windows)]
    return format!("Start-Sleep -Seconds {seconds}");
    #[cfg(not(windows))]
    format!("sleep {seconds}")
}
impl Daemon {
    async fn spawn(root: &Path, ops: &[&str]) -> Self {
        let config = root.join("project/.cortexkit");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("aft.jsonc"),
            serde_json::to_vec(
                &json!({"indexes":{"callgraph":false,"trigram":false,"semantic":false}}),
            )
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "scope-tests".into(),
        };
        let connection_path = root.join("connection.json");
        connection_file::write_atomic(&connection_path, &connection).unwrap();
        let mut command = Command::new(aft_binary::aft_binary());
        let stderr_path = root.join("daemon.stderr");
        #[cfg(feature = "test-timing-hooks")]
        let describe_clock = tempfile::Builder::new()
            .prefix("describe-clock-")
            .tempdir_in(root)
            .unwrap();
        #[cfg(feature = "test-timing-hooks")]
        {
            command.env("AFT_TEST_SCOPE_DESCRIBE_CLOCK_DIR", describe_clock.path());
        }
        command
            .current_dir(root)
            .arg("--subc")
            .arg(&connection_path)
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("AFT_STORAGE_DIR")
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .env("AFT_TEST_SCOPE_POLL_MS", "500")
            .env("AFT_TEST_SCOPE_DESCRIBE_TIMEOUT_MS", "200")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
        for (variable, directory) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let path = root.join(directory);
            std::fs::create_dir_all(&path).unwrap();
            command.env(variable, path);
        }
        let child = command.spawn().unwrap();
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        authenticate_server(
            &mut socket,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "scope-tests",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut socket).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: ops.iter().map(|op| (*op).into()).collect(),
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut socket,
            &Frame::build(
                FrameType::HelloAck,
                flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let (mut read, writer) = socket.into_split();
        let (tx, frames) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut read).await {
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            root: root.into(),
            child,
            writer,
            frames,
            reader,
            parked: VecDeque::new(),
            corr: 100,
            controls: vec![],
            stderr_path,
            frame_trace: Vec::new(),
            #[cfg(feature = "test-timing-hooks")]
            describe_clock,
        }
    }
    async fn send(
        &mut self,
        ty: FrameType,
        channel: u16,
        epoch: u32,
        corr: u64,
        value: Option<Value>,
    ) {
        let bytes = value
            .map(|value| serde_json::to_vec(&value).unwrap())
            .unwrap_or_default();
        self.frame_trace.push(format!(
            "at_ms={} sent type={ty:?} channel={channel} epoch={epoch} corr={corr} body={}",
            aft::bash_background::persistence::unix_millis(),
            String::from_utf8_lossy(&bytes)
        ));
        write_frame(
            &mut self.writer,
            &Frame::build(ty, flags(), channel, epoch, corr, bytes).unwrap(),
        )
        .await
        .unwrap();
    }
    async fn next_matching(
        &mut self,
        within: Duration,
        matches: impl Fn(&Frame) -> bool,
    ) -> Option<Frame> {
        if let Some(index) = self.parked.iter().position(&matches) {
            return self.parked.remove(index);
        }
        let until = tokio::time::Instant::now() + within;
        loop {
            let frame = tokio::time::timeout_at(until, self.frames.recv())
                .await
                .ok()??;
            self.frame_trace.push(format!(
                "at_ms={} received type={:?} channel={} epoch={} corr={} body={}",
                aft::bash_background::persistence::unix_millis(),
                frame.header.ty,
                frame.header.channel,
                frame.header.epoch,
                frame.header.corr,
                String::from_utf8_lossy(&frame.body)
            ));
            if frame.header.ty == FrameType::Ping {
                self.send(FrameType::Pong, 0, 0, frame.header.corr, None)
                    .await;
                continue;
            }
            if frame.header.ty == FrameType::Push {
                continue;
            }
            if frame.header.ty == FrameType::Request && frame.header.channel == 0 {
                let op = body(&frame)["op"].as_str().unwrap_or("").to_owned();
                self.controls.push((frame.header.corr, op.clone()));
                if matches!(op.as_str(), "supervisor.live_roots" | "catalog.update") {
                    let response = if op == "supervisor.live_roots" {
                        json!({"op":"supervisor.live_roots","roots":[],"unknown_root_bindings":0,"total_bindings":0})
                    } else {
                        json!({"op":"catalog.update"})
                    };
                    self.send(FrameType::Response, 0, 0, frame.header.corr, Some(response))
                        .await;
                    continue;
                }
            }
            if matches(&frame) {
                return Some(frame);
            }
            self.parked.push_back(frame);
        }
    }
    async fn bind(
        &mut self,
        channel: u16,
        bind_epoch: u32,
        principal: &str,
        scope: Option<subc_protocol::scope::ScopeStamp>,
        legacy: bool,
    ) {
        let session = format!("session-{channel}");
        self.bind_route(
            channel, bind_epoch, principal, scope, legacy, &session, !legacy,
        )
        .await;
    }
    /// `probe_ready` waits for aft.db through `late_results`; routes that may
    /// only call one operation (such as exact-read routes) skip that probe.
    #[allow(clippy::too_many_arguments)]
    async fn bind_route(
        &mut self,
        channel: u16,
        bind_epoch: u32,
        principal: &str,
        scope: Option<subc_protocol::scope::ScopeStamp>,
        legacy: bool,
        session: &str,
        probe_ready: bool,
    ) {
        self.corr += 1;
        let corr = self.corr;
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: bind_epoch,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(&self.root.join("project"), "runner", session),
            principal: Some(if principal == "direct" {
                Principal::Direct
            } else {
                Principal::Reserved {
                    module_id: principal.into(),
                }
            }),
            consumer_capabilities: Some(vec!["elicitation".into()]),
            admission_facts: None,
            scope,
            role_versions: (!legacy)
                .then(|| BTreeMap::from([("tool-provider".into(), "v1".into())])),
            call_mode: None,
        };
        self.send(
            FrameType::Request,
            0,
            0,
            corr,
            Some(serde_json::to_value(bind).unwrap()),
        )
        .await;
        let ack = self
            .next_matching(Duration::from_secs(15), |frame| {
                frame.header.channel == 0
                    && frame.header.corr == corr
                    && frame.header.ty != FrameType::Request
            })
            .await
            .expect("bind reply");
        assert_eq!(ack.header.ty, FrameType::Response, "{}", body(&ack));
        if probe_ready {
            // Bind acknowledgement precedes asynchronous aft.db opening.
            // late_results must become available before submitting shell calls;
            // this readiness probe consumes no call key or ledger row.
            let until = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                let corr = self
                    .start(
                        channel,
                        bind_epoch,
                        json!({"name":"late_results","arguments":{}}),
                    )
                    .await;
                let ready = self.reply(channel, corr).await;
                if ready.header.ty == FrameType::Response {
                    break;
                }
                assert_eq!(
                    body(&ready)["code"],
                    "database_initializing",
                    "{}",
                    body(&ready)
                );
                assert!(tokio::time::Instant::now() < until);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    async fn start(&mut self, channel: u16, epoch: u32, request: Value) -> u64 {
        self.corr += 1;
        let corr = self.corr;
        self.send(FrameType::Request, channel, epoch, corr, Some(request))
            .await;
        corr
    }
    async fn reply(&mut self, channel: u16, corr: u64) -> Frame {
        self.next_matching(Duration::from_secs(15), |frame| {
            frame.header.channel == channel
                && frame.header.corr == corr
                && matches!(frame.header.ty, FrameType::Error | FrameType::Response)
        })
        .await
        .unwrap_or_else(|| panic!("terminal reply timed out for channel={channel} corr={corr}; all sent/received frames:\n{}\ndaemon stderr (including tail):\n{}",
            self.frame_trace.join("\n"), std::fs::read_to_string(&self.stderr_path).unwrap_or_else(|error| error.to_string())))
    }
    async fn call(&mut self, channel: u16, request: Value) -> Frame {
        let corr = self.start(channel, 1, request).await;
        self.reply(channel, corr).await
    }
    // The fixture's owner/carrier modules are absent from the first-party
    // allowlist. Untrusted callers require strict regardless of configured role
    // profiles, so Windows must refuse their shells before creating a task.
    async fn assert_windows_strict_refusal(&mut self, channel: u16, request: Value) {
        let corr = self.start(channel, 1, request).await;
        let first = self
            .next_matching(Duration::from_secs(10), |frame| {
                frame.header.channel == channel
                    && (frame.header.ty == FrameType::Request || frame.header.corr == corr)
            })
            .await
            .expect("strict shell returned neither a permission question nor a terminal reply");
        if first.header.ty == FrameType::Request {
            self.send(
                FrameType::Response,
                channel,
                first.header.epoch,
                first.header.corr,
                Some(json!({"decision":"allow"})),
            )
            .await;
        } else {
            self.parked.push_back(first);
        }
        let reply = self.reply(channel, corr).await;
        assert_eq!(reply.header.ty, FrameType::Response, "{}", body(&reply));
        assert_eq!(
            body(&reply),
            json!({
                "content":[{"type":"text","text":"sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"}],
                "isError":true
            })
        );
        assert!(task_metadata(&self.root).is_empty());
    }
    async fn ask(&mut self, channel: u16) -> Frame {
        self.next_matching(Duration::from_secs(10), |frame| {
            frame.header.channel == channel && frame.header.ty == FrameType::Request
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "permission question; parked frames: {:?}",
                self.parked.iter().map(body).collect::<Vec<_>>()
            )
        })
    }
    async fn allow(&mut self, channel: u16) {
        let ask = self.ask(channel).await;
        self.send(
            FrameType::Response,
            channel,
            ask.header.epoch,
            ask.header.corr,
            Some(json!({"decision":"allow"})),
        )
        .await;
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn describe(&mut self) -> Frame {
        self.next_matching(Duration::from_secs(3), |frame| {
            frame.header.channel == 0
                && frame.header.ty == FrameType::Request
                && body(frame)["op"] == "scope.describe"
        })
        .await
        .expect("scope.describe")
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn answer(&mut self, request: &Frame, status: &str, synced: bool, epoch: u64) {
        self.send(FrameType::Response, 0, 0, request.header.corr, Some(json!({"op":"scope.describe", "status":status,"scope_epoch":epoch,"owner_synced":synced,"owner_configured":true,"daemon_incarnation":"test-daemon"}))).await;
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn wait_describe_clock(&self, event: &str, corr: u64) {
        let path = self.describe_clock.path().join(format!("{event}-{corr}"));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("scope describe clock did not report {event} for correlation {corr}")
        });
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn expire_describe(&self, request: &Frame) {
        std::fs::write(
            self.describe_clock
                .path()
                .join(format!("expire-{}", request.header.corr)),
            b"expire",
        )
        .unwrap();
        self.wait_describe_clock("expired", request.header.corr)
            .await;
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn answer_after_describe_budget(
        &mut self,
        request: &Frame,
        status: &str,
        synced: bool,
        epoch: u64,
    ) {
        // Keep the reply held until the old wall budget has actually elapsed,
        // even when process startup is fast. The correlation must remain live.
        self.wait_describe_clock("elapsed", request.header.corr)
            .await;
        self.answer(request, status, synced, epoch).await;
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn goodbye(&mut self, channel: u16, epoch: u32) {
        self.send(FrameType::Goodbye, channel, epoch, 0, None).await;
    }
    async fn drain(&mut self) {
        self.send(FrameType::Push, 0, 0, 0, Some(json!({"op":"module.draining","reason":"restart","deadline_ms":aft::bash_background::persistence::unix_millis()+5000}))).await;
    }
    async fn status(&mut self, channel: u16, task: &str) -> String {
        let frame = self
            .call(
                channel,
                json!({"name":"bash_status","preset":"head","arguments":{"taskId":task}}),
            )
            .await;
        let value = body(&frame);
        value["content"][0]["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    }
    async fn wait_status(&mut self, channel: u16, task: &str, expected: &str) -> String {
        let until = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let text = self.status(channel, task).await;
            if text.contains(expected) {
                return text;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "expected {expected}: {text}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
}
fn shell(key: Option<&str>, background: bool, seconds: u64) -> Value {
    let mut call = json!({"name":"bash","preset":"head","arguments":{"command":sleep_command(seconds),"background":background,"timeout":20000}});
    if let Some(key) = key {
        call["call_key"] = json!(key);
    }
    call
}

fn gated_shell(root: &Path, key: Option<&str>) -> Value {
    let mut call = shell(key, false, 15);
    let release = root
        .join("project/release")
        .display()
        .to_string()
        .replace('\\', "/");
    #[cfg(windows)]
    let command = format!(
        "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 50 }}",
        release.replace('\'', "''")
    );
    #[cfg(not(windows))]
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done",
        release.replace('\'', "'\\''")
    );
    call["arguments"]["command"] = json!(command);
    call
}

#[tokio::test]
async fn drain_detaches_keyed_and_keyless_calls_and_retains_only_real_exit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon.bind(2, 1, "direct", None, false).await;
    if cfg!(windows) {
        daemon
            .assert_windows_strict_refusal(1, gated_shell(&root, Some("drained")))
            .await;
        return;
    }
    let keyed = daemon
        .start(1, 1, gated_shell(&root, Some("drained")))
        .await;
    daemon.allow(1).await;
    let plain = daemon.start(2, 1, gated_shell(&root, None)).await;
    running_task(&mut daemon, 1, keyed).await;
    running_task(&mut daemon, 2, plain).await;
    daemon.drain().await;
    let mut tasks = vec![];
    for (channel, corr, late) in [(1, keyed, true), (2, plain, false)] {
        let reply = daemon.reply(channel, corr).await;
        assert_eq!(reply.header.ty, FrameType::Error, "{}", body(&reply));
        let response = body(&reply);
        assert_eq!(response["code"], "detached", "{response}");
        assert_eq!(response["message"], "call detached; command still running");
        assert_eq!(response["detail"]["late_results"], late);
        tasks.push(response["detail"]["task_id"].as_str().unwrap().to_owned());
    }
    for task in &tasks {
        let records = task_metadata(&root);
        let record = &records
            .iter()
            .find(|(_, record)| record["task_id"] == *task)
            .unwrap()
            .1;
        assert_eq!(record["status"], "running", "{record}");
    }
    assert!(daemon
        .next_matching(Duration::from_millis(100), |frame| frame.header.ty
            == FrameType::Request
            && frame.header.channel == 0)
        .await
        .is_none());
    std::fs::write(root.join("project/release"), "release").unwrap();
    drop(daemon);
    let mut daemon = Daemon::spawn(&root, &[]).await;
    daemon
        .bind(1, 1, "direct", Some(stamp("owner", 1)), false)
        .await;
    daemon.bind(2, 1, "direct", None, false).await;
    daemon
        .bind(3, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    for (index, task) in tasks.iter().enumerate() {
        daemon
            .wait_status(index as u16 + 1, task, "completed (exit 0)")
            .await;
    }
    let page = body(
        &daemon
            .call(3, json!({"name":"late_results","arguments":{}}))
            .await,
    );
    assert_eq!(page["entries"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["entries"][0]["call_key"], "drained");
    assert_eq!(page["entries"][0]["kind"], "result");
    assert!(
        !page.to_string().contains("\"code\":\"detached\""),
        "{page}"
    );
    daemon
        .call(
            3,
            json!({"name":"late_results.ack","arguments":{"through":page["cursor"]}}),
        )
        .await;
    let page = body(
        &daemon
            .call(3, json!({"name":"late_results","arguments":{}}))
            .await,
    );
    assert_eq!(page["entries"], json!([]));
}

fn task_metadata(root: &Path) -> Vec<(PathBuf, Value)> {
    fn visit(directory: &Path, out: &mut Vec<(PathBuf, Value)>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else if path.file_name().and_then(|name| name.to_str()) == Some("metadata.json") {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        out.push((path, value));
                    }
                }
            }
        }
    }
    let mut tasks = vec![];
    visit(root, &mut tasks);
    tasks
}
async fn running_task(daemon: &mut Daemon, channel: u16, corr: u64) -> (PathBuf, Value) {
    let session = format!("session-{channel}");
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(task) = task_metadata(&daemon.root)
            .into_iter()
            .find(|(_, task)| task["session_id"] == session && task["status"] == "running")
        {
            return task;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "running task for {session} never persisted: {:?}",
            task_metadata(&daemon.root)
        );
        // A refused command has no task record. Read its terminal reply rather
        // than polling the filesystem until a generic startup timeout hides it.
        if let Some(reply) = daemon
            .next_matching(Duration::from_millis(20), |frame| {
                frame.header.channel == channel
                    && frame.header.corr == corr
                    && (frame.header.ty == FrameType::Error
                        || (frame.header.ty == FrameType::Response
                            && body(frame)["isError"] == true))
            })
            .await
        {
            panic!("shell task never started for {session}: {}", body(&reply));
        }
    }
}

#[tokio::test]
#[should_panic(expected = "shell task never started for session-1: {\"code\":\"invalid_request\"")]
async fn refused_shell_start_fails_fast_with_the_terminal_reason() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &[]).await;
    daemon.bind(1, 1, "direct", None, false).await;
    let mut request = shell(None, false, 15);
    request["arguments"]["env"] = json!({"PATH":"blocked"});
    let corr = daemon.start(1, 1, request).await;
    // The refusal must arrive as its own terminal reply, well before the 15 s
    // startup timeout; 10 s keeps that proof while a loaded macOS runner, which
    // took over 2 s, still fits.
    tokio::time::timeout(Duration::from_secs(10), running_task(&mut daemon, 1, corr))
        .await
        .expect("startup refusal was ignored instead of reporting its terminal reason");
}

#[tokio::test]
#[should_panic(expected = "\"isError\":true")]
async fn refused_untrusted_shell_start_fails_fast_with_the_text_error() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &[]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    let mut request = shell(Some("refused-text"), false, 15);
    request["arguments"]["workdir"] = json!(root.join("missing-workdir"));
    let corr = daemon.start(1, 1, request).await;
    daemon.allow(1).await;
    tokio::time::timeout(Duration::from_secs(2), running_task(&mut daemon, 1, corr))
        .await
        .expect("text-only shell refusal was ignored instead of reporting its terminal reason");
}

#[cfg(windows)]
#[tokio::test]
async fn first_party_worker_bash_runs_with_default_relaxed_and_reports_unenforced_denies() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &[]).await;
    std::fs::write(
        root.join("project/.cortexkit/aft.jsonc"),
        serde_json::to_vec(&json!({
            "indexes":{"callgraph":false,"trigram":false,"semantic":false},
            "bash":{"rewrite":true}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("project/worker-proof.txt"), "relaxed-worker-ran").unwrap();
    let scope = serde_json::from_value(json!({
        "owner": {"kind":"reserved","module_id":"prefrontal-core"},
        "ref":"worker-scope", "scope_epoch":1, "kind":"worker",
        "parent": {"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"head-scope","scope_epoch":1},
        "owner_authorized":true
    })).unwrap();
    daemon.bind(1, 1, "broca", Some(scope), false).await;
    for command in ["echo relaxed-worker-ran", "cat worker-proof.txt"] {
        let reply = daemon
            .call(1, json!({"name":"bash","preset":"worker","arguments":{"command":command,"compressed":false,"timeout":10000}}))
            .await;
        assert_eq!(reply.header.ty, FrameType::Response, "{}", body(&reply));
        let output = body(&reply).to_string();
        assert!(output.contains("relaxed-worker-ran"), "{output}");
        assert!(
            output.contains(
                "profile=relaxed unenforced=[all filesystem read/write denies,operator gh fence: unenforced]"
            ),
            "{output}"
        );
        assert!(
            output.contains("command runs with full user rights"),
            "{output}"
        );
    }
    assert_eq!(
        task_metadata(&root).len(),
        2,
        "bash must execute, not rewrite"
    );
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn goodbye_ended_kills_exact_owner_and_refuses_prepared_call() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner-a", Some(stamp("owner-a", 1)), false)
        .await;
    daemon
        .bind(2, 1, "owner-b", Some(stamp("owner-b", 1)), false)
        .await;
    daemon
        .bind(3, 1, "owner-a", Some(stamp("owner-a", 1)), false)
        .await;
    if cfg!(windows) {
        daemon
            .assert_windows_strict_refusal(1, shell(Some("first"), false, 15))
            .await;
        daemon
            .assert_windows_strict_refusal(2, shell(Some("other"), true, 15))
            .await;
        return;
    }
    let _first = daemon.start(1, 1, shell(Some("first"), false, 15)).await;
    daemon.allow(1).await;
    let _other = daemon.start(2, 1, shell(Some("other"), true, 15)).await;
    daemon.allow(2).await;
    let first = running_task(&mut daemon, 1, _first).await.1;
    let other = running_task(&mut daemon, 2, _other).await.1;
    assert_eq!(first["scope"]["owner"]["module_id"], "owner-a");
    let held = daemon.start(3, 1, shell(Some("prepared"), false, 15)).await;
    daemon.ask(3).await;
    daemon.goodbye(1, 1).await;
    let describe = daemon.describe().await;
    assert_eq!(body(&describe)["owner"]["module_id"], "owner-a");
    daemon.answer(&describe, "ended", true, 1).await;
    let refused = body(&daemon.reply(3, held).await);
    assert_eq!(refused["code"], "not_started", "{refused}");
    assert_eq!(refused["detail"]["reason"], "scope_ended");
    let withdraw = body(
        &daemon
            .call(
                3,
                json!({"name":"tool.withdraw","arguments":{"call_key":"prepared"}}),
            )
            .await,
    );
    assert_eq!(withdraw, json!({"answer":"refused","reason":"scope_ended"}));
    let task = first["task_id"].as_str().unwrap();
    daemon
        .bind(4, 1, "owner-a", Some(stamp("owner-a", 1)), false)
        .await;
    // Rebind channel 1 to session-1: bash_status only exposes tasks
    // started in the caller's session.
    daemon
        .bind(1, 2, "direct", Some(stamp("owner-a", 1)), false)
        .await;
    let corr = daemon
        .start(
            1,
            2,
            json!({"name":"bash_status","preset":"head","arguments":{"taskId":task}}),
        )
        .await;
    let status = body(&daemon.reply(1, corr).await).to_string();
    assert!(
        status.contains("killed") && status.contains("scope_revoked"),
        "{status}"
    );
    assert_eq!(
        task_metadata(&root)
            .iter()
            .find(|(_, task)| task["task_id"] == other["task_id"])
            .unwrap()
            .1["status"],
        "running"
    );
    daemon.send(FrameType::Cancel, 2, 1, _other, None).await;
    assert_eq!(body(&daemon.reply(2, _other).await)["code"], "cancelled");
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn nondefinitive_answers_retry_without_killing_and_detached_result_is_once() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "carrier", Some(stamp("owner", 1)), false)
        .await;
    if cfg!(windows) {
        daemon
            .assert_windows_strict_refusal(1, gated_shell(&root, Some("natural")))
            .await;
        return;
    }
    let corr = daemon
        .start(1, 1, gated_shell(&root, Some("natural")))
        .await;
    daemon.allow(1).await;
    let task = running_task(&mut daemon, 1, corr).await.1;
    daemon
        .bind(2, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon
        .bind(3, 1, "carrier", Some(stamp("owner", 1)), false)
        .await;
    daemon.goodbye(1, 1).await;
    let query = daemon.describe().await;
    daemon.answer(&query, "not_live", false, 1).await;
    let query = daemon.describe().await;
    daemon
        .send(
            FrameType::Error,
            0,
            0,
            query.header.corr,
            Some(json!({"code":"busy","message":"not definitive"})),
        )
        .await;
    let timed_out = daemon.describe().await;
    daemon.expire_describe(&timed_out).await;
    // An expired request holds work; even its late Ended reply must not kill it.
    daemon.answer(&timed_out, "ended", true, 1).await;
    let retry = daemon.describe().await;
    assert_ne!(timed_out.header.corr, retry.header.corr);
    daemon.answer(&retry, "live", true, 2).await;
    assert_eq!(
        task_metadata(&root)
            .iter()
            .find(|(_, value)| value["task_id"] == task["task_id"])
            .unwrap()
            .1["status"],
        "running"
    );
    std::fs::write(root.join("project/release"), "release").unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(8);
    let page = loop {
        let page = body(
            &daemon
                .call(2, json!({"name":"late_results","arguments":{}}))
                .await,
        );
        if !page["entries"].as_array().unwrap().is_empty() {
            break page;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "late result never arrived: {page}, tasks={:?}",
            task_metadata(&root)
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["call_key"], "natural");
    assert_eq!(page["entries"][0]["kind"], "result");
    // The module named "carrier" is not in AFT's trusted consumer list,
    // so its wire result omits structured status. Empty command output
    // renders only the task marker; the persisted task record proves the exit.
    let completed = task_metadata(&root)
        .into_iter()
        .find(|(_, record)| record["task_id"] == task["task_id"])
        .unwrap()
        .1;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exit_code"], 0);
    let carrier = body(
        &daemon
            .call(3, json!({"name":"late_results","arguments":{}}))
            .await,
    );
    assert_eq!(carrier["entries"], json!([]));
    daemon
        .call(
            2,
            json!({"name":"late_results.ack","arguments":{"through":page["cursor"]}}),
        )
        .await;
    assert_eq!(
        body(
            &daemon
                .call(2, json!({"name":"late_results","arguments":{}}))
                .await
        )["entries"],
        json!([])
    );
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn rebind_live_epoch_move_survives_then_old_answer_spares_new_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(
        &root,
        &["scope.describe", "catalog.update", "supervisor.live_roots"],
    )
    .await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    if cfg!(windows) {
        daemon
            .assert_windows_strict_refusal(1, shell(Some("old"), false, 15))
            .await;
        return;
    }
    let corr = daemon.start(1, 1, shell(Some("old"), false, 15)).await;
    daemon.allow(1).await;
    let old = running_task(&mut daemon, 1, corr).await.1;
    daemon
        .bind(1, 2, "owner", Some(stamp("owner", 2)), false)
        .await;
    let poll = daemon.describe().await;
    daemon.answer(&poll, "live", true, 2).await;
    assert_eq!(
        task_metadata(&root)
            .iter()
            .find(|(_, task)| task["task_id"] == old["task_id"])
            .unwrap()
            .1["status"],
        "running"
    );
    let poll = daemon.describe().await;
    let current = daemon.start(1, 2, shell(Some("new"), true, 15)).await;
    daemon.allow(1).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if task_metadata(&root)
            .iter()
            .any(|(_, task)| task["scope"]["scope_epoch"] == 2 && task["status"] == "running")
        {
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon
        .answer_after_describe_budget(&poll, "ended", true, 1)
        .await;
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if task_metadata(&root)
            .iter()
            .any(|(_, task)| task["task_id"] == old["task_id"] && task["status"] == "killed")
        {
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let tasks = task_metadata(&root);
    let new = &tasks
        .iter()
        .find(|(_, task)| task["scope"]["scope_epoch"] == 2)
        .unwrap()
        .1;
    assert_eq!(new["status"], "running");
    assert_eq!(
        tasks
            .iter()
            .find(|(_, task)| task["task_id"] == old["task_id"])
            .unwrap()
            .1["status_reason"],
        "scope_revoked"
    );
    let mut corrs = std::collections::HashSet::new();
    for (corr, _) in &daemon.controls {
        assert!(
            corrs.insert(corr),
            "control corr reused: {:?}",
            daemon.controls
        );
    }
    assert!(daemon.controls.iter().any(|(_, op)| op == "catalog.update"));
    daemon.send(FrameType::Cancel, 1, 2, current, None).await;
    assert_eq!(body(&daemon.reply(1, current).await)["code"], "cancelled");
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn goodbye_without_tasks_and_nonadvertising_daemon_send_no_describe() {
    for ops in [vec!["scope.describe"], vec![]] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("module");
        let mut daemon = Daemon::spawn(&root, &ops).await;
        daemon
            .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
            .await;
        if ops.is_empty() {
            if cfg!(windows) {
                daemon
                    .assert_windows_strict_refusal(1, shell(Some("unadvertised"), false, 3))
                    .await;
                continue;
            }
            let call = daemon
                .start(1, 1, shell(Some("unadvertised"), false, 3))
                .await;
            daemon.allow(1).await;
            running_task(&mut daemon, 1, call).await;
            // Without scope.describe advertisement no request is sent.
            // Closing the route alone still cannot terminate the command.
            daemon.goodbye(1, 1).await;
            assert!(daemon
                .next_matching(Duration::from_millis(700), |frame| frame.header.ty
                    == FrameType::Request
                    && frame.header.channel == 0)
                .await
                .is_none());
            assert_eq!(task_metadata(&root)[0].1["status"], "running");
            assert!(daemon
                .next_matching(Duration::from_millis(50), |frame| frame.header.channel == 1
                    && frame.header.corr == call)
                .await
                .is_none());
        } else {
            daemon.goodbye(1, 1).await;
            assert!(daemon
                .next_matching(Duration::from_millis(700), |frame| frame.header.ty
                    == FrameType::Request
                    && frame.header.channel == 0)
                .await
                .is_none());
        }
    }
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn restart_recovers_stamped_task_and_polls_only_after_interval() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    let marker = root.join("project/started");
    let mut call = gated_shell(&root, Some("recovered"));
    call["arguments"]["timeout"] = json!(120000);
    if cfg!(windows) {
        daemon.assert_windows_strict_refusal(1, call).await;
        return;
    }
    #[cfg(windows)]
    let signal = format!(
        "Set-Content -LiteralPath '{}' -Value ready",
        marker.display().to_string().replace('\'', "''")
    );
    #[cfg(not(windows))]
    let signal = format!(
        "printf ready > '{}'",
        marker.display().to_string().replace('\'', "'\\''")
    );
    call["arguments"]["command"] = json!(format!(
        "{signal}; {}",
        call["arguments"]["command"].as_str().unwrap()
    ));
    let corr = daemon.start(1, 1, call).await;
    daemon.allow(1).await;
    let task = running_task(&mut daemon, 1, corr).await.1;
    // A running metadata record can precede wrapper startup. Wait for the
    // command itself, and gate its exit so restart latency cannot finish it.
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            tokio::time::Instant::now() < until,
            "recovery command never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(daemon);
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    // Recover the original session under a new epoch without revoking it.
    daemon
        .bind(1, 1, "direct", Some(stamp("owner", 2)), false)
        .await;
    // Opening aft.db does not acknowledge restoration of task records
    // into the registry. Await bash_status confirming the live command.
    daemon
        .wait_status(1, task["task_id"].as_str().unwrap(), "running")
        .await;
    let poll = daemon.describe().await;
    daemon.answer(&poll, "not_live", true, 1).await;
    let status = daemon
        .wait_status(1, task["task_id"].as_str().unwrap(), "killed")
        .await;
    assert!(status.contains("scope revoked"), "{status}");
}

#[tokio::test]
async fn explicit_cancel_kills_while_keyless_and_legacy_create_no_ledger_rows() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &[]).await;
    daemon.bind(1, 1, "direct", None, false).await;
    daemon.bind(2, 1, "direct", None, true).await;
    let cancel = daemon.start(1, 1, shell(None, false, 15)).await;
    let task = running_task(&mut daemon, 1, cancel).await.1;
    daemon.send(FrameType::Cancel, 1, 1, cancel, None).await;
    assert_eq!(body(&daemon.reply(1, cancel).await)["code"], "cancelled");
    daemon
        .wait_status(1, task["task_id"].as_str().unwrap(), "killed")
        .await;
    let legacy = daemon.call(2, shell(Some("legacy-key"), true, 1)).await;
    assert_eq!(legacy.header.ty, FrameType::Response, "{}", body(&legacy));
    drop(daemon);
    fn database(path: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(path).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = database(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| name == "aft.db") {
                return Some(path);
            }
        }
        None
    }
    let conn = rusqlite::Connection::open(database(&root).unwrap()).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |row| row
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[cfg(all(windows, feature = "test-timing-hooks"))]
#[tokio::test]
async fn powershell_scope_route_refuses_unenforced_strict_before_start() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    let mut request = shell(Some("powershell"), true, 15);
    request["name"] = json!("powershell");
    request["arguments"]["command"] = json!("Start-Sleep -Seconds 15");
    daemon.assert_windows_strict_refusal(1, request).await;
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn connection_loss_detaches_without_queueing_or_killing() {
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    if cfg!(windows) {
        daemon
            .assert_windows_strict_refusal(1, shell(Some("lost"), false, 15))
            .await;
        return;
    }
    let corr = daemon.start(1, 1, shell(Some("lost"), false, 15)).await;
    daemon.allow(1).await;
    let task = running_task(&mut daemon, 1, corr).await.1;
    daemon.writer.shutdown().await.unwrap();
    daemon.reader.abort();
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(
            tokio::time::Instant::now() < until,
            "module did not finish connection-loss shutdown"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(daemon.controls.is_empty());
    assert_eq!(task_metadata(&root)[0].1["status"], "running");
    drop(daemon);
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "direct", Some(stamp("owner", 1)), false)
        .await;
    assert!(daemon
        .status(1, task["task_id"].as_str().unwrap())
        .await
        .contains("running"));
    assert!(daemon
        .next_matching(Duration::from_millis(700), |frame| frame.header.ty
            == FrameType::Request
            && frame.header.channel == 0)
        .await
        .is_none());
    daemon
        .call(
            1,
            json!({"name":"bash_kill","preset":"head","arguments":{"taskId":task["task_id"]}}),
        )
        .await;
}

// Basal reads exact files over an ordinary tool-provider/v1 route to AFT,
// bound with the head's bind identity under a `flow-read/` child of the head's
// scope, sending the flat `{"op":"file.read_exact","path":...}` body rather
// than a `{name, arguments, call_key}` tool-call envelope.
fn flow_owner() -> Principal {
    Principal::Reserved {
        module_id: "prefrontal-core".into(),
    }
}
fn flow_head_stamp() -> subc_protocol::scope::ScopeStamp {
    subc_protocol::scope::ScopeStamp::new(
        flow_owner(),
        "head",
        7,
        subc_protocol::scope::ScopeKind::Head,
    )
    .with_owner_authorized(true)
}
fn flow_read_stamp() -> subc_protocol::scope::ScopeStamp {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    subc_protocol::scope::ScopeStamp::new(
        flow_owner(),
        "flow-read/call-1",
        1,
        subc_protocol::scope::ScopeKind::Ephemeral,
    )
    .with_owner_authorized(true)
    .with_expires_at_ms(Some(now + 60_000))
    .with_parent(Some(subc_protocol::scope::ScopeParent::new(
        flow_owner(),
        "head",
        7,
    )))
}

#[tokio::test]
async fn flow_read_scope_on_tool_provider_v1_route_returns_exact_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    std::fs::write(root.join("project/flow-script.txt"), b"abc").unwrap();
    daemon
        .bind_route(
            1,
            1,
            "direct",
            Some(flow_head_stamp()),
            false,
            "head-session",
            true,
        )
        .await;
    // The read checks the head's stored policy, which is registered after the
    // bind is acknowledged. Nothing here orders that registration before the
    // read: the read itself must wait for it.
    daemon
        .bind_route(
            2,
            1,
            "basal",
            Some(flow_read_stamp()),
            false,
            "head-session",
            false,
        )
        .await;
    let corr = daemon
        .start(
            2,
            1,
            json!({"op":"file.read_exact","path":"flow-script.txt"}),
        )
        .await;
    // AFT confirms the parent head scope is live with the daemon before reading.
    let describe = daemon
        .next_matching(Duration::from_secs(10), |frame| {
            frame.header.channel == 0
                && frame.header.ty == FrameType::Request
                && body(frame)["op"] == "scope.describe"
        })
        .await
        .expect("parent scope.describe");
    assert_eq!(body(&describe)["ref"], "head", "{}", body(&describe));
    let live = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
        status: subc_protocol::scope::ScopeStatus::Live,
        scope_epoch: Some(7),
        scope: Some(flow_head_stamp()),
        daemon_incarnation: "test-daemon".into(),
        owner_synced: true,
        owner_configured: true,
    };
    daemon
        .send(
            FrameType::Response,
            0,
            0,
            describe.header.corr,
            Some(serde_json::to_value(live).unwrap()),
        )
        .await;
    let reply = daemon.reply(2, corr).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", body(&reply));
    assert_eq!(
        body(&reply),
        json!({
            "bytes_base64":"YWJj",
            "size":3,
            "sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        })
    );
}

#[tokio::test]
async fn file_read_exact_on_tool_provider_v1_route_without_flow_scope_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    std::fs::write(root.join("project/flow-script.txt"), b"abc").unwrap();
    daemon
        .bind_route(2, 1, "basal", None, false, "head-session", true)
        .await;
    let reply = daemon
        .call(2, json!({"op":"file.read_exact","path":"flow-script.txt"}))
        .await;
    assert_eq!(reply.header.ty, FrameType::Error, "{}", body(&reply));
    assert_eq!(body(&reply)["code"], "file_read_exact_route_not_admitted");
    assert_eq!(body(&reply)["message"], "Route not admitted: owner.");
}
