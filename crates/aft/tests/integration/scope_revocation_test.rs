//! Run lifetime tests use real bash children and daemon scope.describe replies
//! that report whether a run is live, ended, or absent after owner synchronization.

use super::*;
use std::path::Path;

async fn open_scope_daemon_session(input: FakeDaemonInput) -> FakeDaemonSession {
    open_fake_daemon_session_with_ops(input, &[subc_protocol::scope::SCOPE_DESCRIBE_OP])
        .await
        .0
}

/// A bash task that a spawn dispatch reported, with the command that started it.
#[derive(Clone)]
struct ObservedTask {
    command: String,
    registry: aft::bash_background::BgTaskRegistry,
    task: String,
}

/// Every bash task `observing_dispatch` has seen in this test process.
///
/// The tests in one process share this list, and an earlier test's entry is
/// still here when a later test runs, so a reader looks up the entry for the
/// command it sent. Each test's command names a file inside its own temporary
/// root, which keeps the entries apart.
static OBSERVED_TASKS: Mutex<Vec<ObservedTask>> = Mutex::new(Vec::new());

fn observing_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    let command = (req.command == "bash").then(|| {
        req.params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    let response = bridge_dispatch(req, ctx);
    if let Some(command) = command {
        if let Some(task) = response.data.get("task_id").and_then(Value::as_str) {
            OBSERVED_TASKS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ObservedTask {
                    command,
                    registry: ctx.bash_background().clone(),
                    task: task.into(),
                });
        }
    }
    response
}

/// The registry and id of the task whose bash command contains `marker`.
///
/// Waits for it, because the child can publish its pid file before the spawn
/// dispatch returns and reports the task: the dispatch still has its
/// foreground wait window and hand-off to finish, and a loaded machine can
/// stretch that past the test's next steps.
async fn observed_task(marker: &Path) -> (aft::bash_background::BgTaskRegistry, String) {
    let marker = marker.display().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = OBSERVED_TASKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|observed| observed.command.contains(&marker))
            .cloned();
        if let Some(observed) = found {
            return (observed.registry, observed.task);
        }
        assert!(
            Instant::now() < deadline,
            "bash dispatch never reported a task for a command naming {marker}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_run_not_killing(marker: &Path) {
    let (registry, task) = observed_task(marker).await;
    let snapshot = registry
        .status(&task, "run-session", None, None, 0)
        .expect("the run's task is readable in its session");
    assert_eq!(
        snapshot.info.status,
        BgTaskStatus::Running,
        "unconfirmed scope close must not start a kill"
    );
}

async fn assert_scope_kill_settled(marker: &Path) {
    assert_scope_kill_settled_as(
        marker,
        "run-session",
        "scope_revoked",
        "killed (scope revoked)",
    )
    .await;
}

/// A codemode program's task killed at its run's end says so.
async fn assert_run_kill_settled(marker: &Path, session: &str) {
    assert_scope_kill_settled_as(marker, session, "run_ended", "killed (run ended)").await;
}

async fn assert_scope_kill_settled_as(marker: &Path, session: &str, reason: &str, rendered: &str) {
    let (registry, task) = observed_task(marker).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = registry.status(&task, session, None, None, 0).unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
            assert_eq!(snapshot.info.status_reason.as_deref(), Some(reason));
            assert!(
                aft::commands::bash_orchestrate::format_foreground_result(&snapshot)
                    .contains(rendered)
            );
            return;
        }
        assert!(Instant::now() < deadline, "scope kill did not settle");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const PREFRONTAL: &str = "prefrontal-core";

/// The head scope a run is opened under: core opens runs only for heads, and
/// a head's scope carries its agent's id.
fn head_stamp(agent: &str) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind":"reserved", "module_id":PREFRONTAL},
        "ref":"head-scope", "scope_epoch":1, "kind":"head",
        "attributes":{"agent_id":agent},
        "owner_authorized":true
    }))
    .unwrap()
}

fn stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind":"reserved", "module_id":PREFRONTAL},
        "ref":"run-scope", "scope_epoch":1, "kind":"ephemeral",
        "attributes":{"agent_id":"agent-a", "run_id":"run-a"},
        "parent": {"owner": {"kind":"reserved", "module_id":PREFRONTAL}, "ref":"head-scope", "scope_epoch":1},
        "parent_state":"linked",
        "owner_authorized":true,
        "expires_at_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 30000
    }))
    .unwrap()
}

/// Bind the head route a codemode run acts for, on its own channel.
async fn bind_head(stream: &mut tokio::net::TcpStream, root: &Path, channel: u16, session: &str) {
    bind_mode_as(
        stream,
        root,
        channel,
        Some(subc_protocol::call_mode::CallMode::Classic),
        Some(head_stamp("agent-a")),
        session,
    )
    .await;
}

async fn bind(stream: &mut tokio::net::TcpStream, root: &Path, channel: u16, run: bool) {
    let mut scope = stamp();
    if !run {
        scope.attributes.run_id = None;
    } else {
        // A run's calls act for its head; the head shares the session so
        // the tasks these tests observe stay where they look for them.
        bind_head(stream, root, channel + 100, "run-session").await;
    }
    bind_mode(
        stream,
        root,
        channel,
        Some(if run {
            subc_protocol::call_mode::CallMode::Codemode
        } else {
            subc_protocol::call_mode::CallMode::Classic
        }),
        Some(scope),
    )
    .await;
}

async fn bind_mode(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    mode: Option<subc_protocol::call_mode::CallMode>,
    scope: Option<subc_protocol::scope::ScopeStamp>,
) {
    bind_mode_as(stream, root, channel, mode, scope, "run-session").await;
}

async fn bind_mode_as(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    mode: Option<subc_protocol::call_mode::CallMode>,
    scope: Option<subc_protocol::scope::ScopeStamp>,
    session: &str,
) {
    bind_route_as(
        stream,
        root,
        channel,
        mode,
        scope,
        session,
        Principal::Direct,
        false,
    )
    .await;
}

/// Bind a route as `principal`, on tool-provider v1 when `v1` is set.
#[allow(clippy::too_many_arguments)]
async fn bind_route_as(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    mode: Option<subc_protocol::call_mode::CallMode>,
    scope: Option<subc_protocol::scope::ScopeStamp>,
    session: &str,
    principal: Principal,
    v1: bool,
) {
    let config_path = root.join(".cortexkit/aft.jsonc");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    if !config_path.exists() {
        std::fs::write(
            config_path,
            serde_json::to_vec(&json!({
                "bash":{"foreground_wait_window_ms":10}
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let body = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(root, "opencode", session),
        principal: Some(principal),
        consumer_capabilities: None,
        admission_facts: Default::default(),
        scope,
        role_versions: v1
            .then(|| std::collections::BTreeMap::from([("tool-provider".into(), "v1".into())])),
        call_mode: mode,
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            channel as u64,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
    expect_route_bind_ack(stream, channel as u64).await;
}

async fn call(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64, args: Value) {
    named_call(stream, channel, corr, "bash", args).await;
}

async fn named_call(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    args: Value,
) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            1,
            corr,
            serde_json::to_vec(&json!({"name":name, "preset":"head", "arguments":args})).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

#[test]
fn call_modes_refuse_flow_writes_and_unknown_but_serve_read_and_classic() {
    run_subc_bridge_test(
        "mode calls",
        Duration::from_secs(30),
        |input| async move {
            use subc_protocol::call_mode::CallMode;
            let mut daemon = open_scope_daemon_session(input).await;
            std::fs::write(daemon.root1.join("mode.txt"), "mode-read-ok").unwrap();
            let mut flow = stamp();
            flow.attributes.flow_id = Some("mode-flow".into());
            flow.attributes.run_id = None;
            flow.expires_at_ms = None;
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(CallMode::Flow),
                Some(flow),
            )
            .await;
            for (corr, name, args) in [
                (100, "bash", json!({"command":"echo forbidden"})),
                (101, "edit", json!({"filePath":"mode.txt","edits":[]})),
                (102, "inspect", json!({})),
            ] {
                named_call(&mut daemon.stream, 1, corr, name, args).await;
                let reply = read_frame_timeout(&mut daemon.stream, "mode refusal").await;
                assert_eq!(reply.header.ty, FrameType::Error);
                let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(
                    error.code,
                    cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
                );
                assert_eq!(error.detail, Some(json!({"tool":name,"mode":"flow"})));
            }
            named_call(
                &mut daemon.stream,
                1,
                103,
                "read",
                json!({"filePath":"mode.txt"}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "flow read").await;
            assert_eq!(
                reply.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            assert!(String::from_utf8_lossy(&reply.body).contains("mode-read-ok"));
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                2,
                Some(CallMode::Unknown("future".into())),
                None,
            )
            .await;
            named_call(
                &mut daemon.stream,
                2,
                104,
                "read",
                json!({"filePath":"mode.txt"}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "unknown mode").await;
            let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(
                error.code,
                cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
            );
            assert_eq!(error.detail, Some(json!({"tool":"read","mode":"future"})));
            bind_mode(&mut daemon.stream, &daemon.root1, 3, None, Some(stamp())).await;
            call(
                &mut daemon.stream,
                3,
                105,
                json!({"command":"echo classic-ok","timeout":1000}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "classic bash").await;
            assert_eq!(reply.header.ty, FrameType::Response);
            assert!(String::from_utf8_lossy(&reply.body).contains("classic-ok"));
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
    );
}

#[test]
fn codemode_bash_refuses_missing_bind_deadline() {
    run_subc_bridge_test(
        "missing run deadline",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            let scope = stamp().with_expires_at_ms(None);
            bind_head(&mut daemon.stream, &daemon.root1, 101, "run-session").await;
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(subc_protocol::call_mode::CallMode::Codemode),
                Some(scope),
            )
            .await;
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":"echo must-not-run","timeout":2000}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "missing deadline refusal").await;
            assert_eq!(reply.header.ty, FrameType::Error);
            let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(error.code, "run_deadline_missing");
            assert_eq!(error.detail, Some(json!({"tool":"bash","mode":"codemode"})));
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
    );
}

static OBSERVED_TIMEOUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn timeout_observing_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    if req.command == "bash" {
        OBSERVED_TIMEOUT.store(
            req.params["timeout"].as_u64().unwrap(),
            std::sync::atomic::Ordering::Release,
        );
    }
    bridge_dispatch(req, ctx)
}

#[test]
fn codemode_bash_clamps_requested_timeout_to_bind_deadline() {
    run_subc_bridge_test_with_dispatch(
        "run deadline clamp",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            // Bind the head first so its bind time is not taken out of the
            // run's five-second budget.
            bind_head(&mut daemon.stream, &daemon.root1, 101, "run-session").await;
            let mut scope = stamp();
            scope.expires_at_ms = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 5000,
            );
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(subc_protocol::call_mode::CallMode::Codemode),
                Some(scope),
            )
            .await;
            OBSERVED_TIMEOUT.store(0, std::sync::atomic::Ordering::Release);
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":"echo clamped","timeout":20000}),
            )
            .await;
            let request = describe(&mut daemon.stream).await;
            answer(
                &mut daemon.stream,
                &request,
                subc_protocol::scope::ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "clamped bash").await;
            assert_eq!(reply.header.ty, FrameType::Response);
            let timeout = OBSERVED_TIMEOUT.load(std::sync::atomic::Ordering::Acquire);
            assert!(
                (1..=5000).contains(&timeout),
                "timeout was {timeout}, must be clamped to bind deadline: {}",
                String::from_utf8_lossy(&reply.body)
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        timeout_observing_dispatch,
    );
}

async fn describe(stream: &mut tokio::net::TcpStream) -> Frame {
    describe_phase(stream, "scope.describe").await
}

async fn describe_phase(stream: &mut tokio::net::TcpStream, phase: &str) -> Frame {
    let request = read_frame_timeout(stream, phase).await;
    assert_eq!(
        request.header.ty,
        FrameType::Request,
        "{phase}: channel={} corr={} body={}",
        request.header.channel,
        request.header.corr,
        String::from_utf8_lossy(&request.body)
    );
    assert_eq!(request.header.channel, 0);
    let body: subc_protocol::session::ModuleControlRequestFromModule =
        serde_json::from_slice(&request.body).unwrap();
    assert!(
        matches!(body, subc_protocol::session::ModuleControlRequestFromModule::ScopeDescribe {owner, scope_ref}
        if owner == stamp().owner && scope_ref == "run-scope")
    );
    request
}

async fn answer(
    stream: &mut tokio::net::TcpStream,
    request: &Frame,
    status: subc_protocol::scope::ScopeStatus,
    synced: bool,
    epoch: Option<u64>,
) {
    let reply = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch: epoch,
        daemon_incarnation: "daemon-a".into(),
        owner_synced: synced,
        owner_configured: true,
        scope: (status == subc_protocol::scope::ScopeStatus::Live).then(|| {
            let mut scope = stamp();
            scope.scope_epoch = epoch.unwrap();
            scope
        }),
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&reply).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, suffix)| suffix.starts_with('Z'))
    }) {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
async fn child_pid(path: &Path) -> u32 {
    // The background shell writes this file once it is running. A loaded CI
    // runner can take several seconds to start it (production allows 15 s),
    // so 5 s failed there; a shell that never starts still fails here.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "bash did not publish grandchild pid"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(all(unix, feature = "test-timing-hooks"))]
async fn wait_test_marker(path: &Path) {
    let until = Instant::now() + Duration::from_secs(5);
    while !std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
        assert!(
            Instant::now() < until,
            "missing test marker {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
async fn no_extra_frames_until_pong(stream: &mut tokio::net::TcpStream, corr: u64, phase: &str) {
    send_frame(
        stream,
        Frame::build(FrameType::Ping, control_flags(), 0, 0, corr, Vec::new()).unwrap(),
    )
    .await;
    let frame = read_frame_timeout(stream, phase).await;
    assert_eq!(
        frame.header.ty,
        FrameType::Pong,
        "{phase}: unexpected frame {:?}",
        frame
    );
    assert_eq!(frame.header.corr, corr, "{phase}");
}

#[cfg(unix)]
async fn wait_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "grandchild {pid} survived the scope end"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
struct ChildCleanup(u32);

#[cfg(all(unix, feature = "test-timing-hooks"))]
struct GateCleanup(std::path::PathBuf);

#[cfg(all(unix, feature = "test-timing-hooks"))]
impl Drop for GateCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0 as i32, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum CloseCase {
    Ended,
    NewEpoch,
    NotLive,
    NotSynced,
    #[cfg(feature = "test-timing-hooks")]
    SlowResync,
    Error,
    Timeout,
    Duplicate,
    NonRun,
}

#[cfg(unix)]
async fn assert_hold_or_retry(
    stream: &mut tokio::net::TcpStream,
    pid: u32,
    marker: &Path,
    reason: &str,
) {
    #[cfg(feature = "test-timing-hooks")]
    let retry = describe(stream).await;
    #[cfg(not(feature = "test-timing-hooks"))]
    assert!(
        read_frame_within(stream, Duration::from_millis(300), reason)
            .await
            .is_none(),
        "production retries must wait for the regular poll"
    );
    assert!(alive(pid), "{reason} must hold work");
    assert_run_not_killing(marker).await;
    #[cfg(feature = "test-timing-hooks")]
    answer(
        stream,
        &retry,
        subc_protocol::scope::ScopeStatus::Ended,
        true,
        Some(1),
    )
    .await;
}

#[cfg(unix)]
fn close_case(name: &'static str, case: CloseCase) {
    run_subc_bridge_test_with_dispatch(
        name,
        Duration::from_secs(30),
        move |input| async move {
            use subc_protocol::scope::ScopeStatus;
            // The bridge fixture serializes environment changes; production
            // builds ignore these knobs and retain their 10 s / 60 s bounds.
            #[cfg(feature = "test-timing-hooks")]
            let _poll = set_test_env(
                "AFT_TEST_SCOPE_POLL_MS",
                if matches!(case, CloseCase::NonRun) {
                    "60000"
                } else {
                    "500"
                },
            );
            #[cfg(feature = "test-timing-hooks")]
            let _timeout = set_test_env(
                "AFT_TEST_SCOPE_DESCRIBE_TIMEOUT_MS",
                if matches!(case, CloseCase::Duplicate) {
                    "10000"
                } else {
                    "1000"
                },
            );
            let mut daemon = open_scope_daemon_session(input).await;
            let run = !matches!(case, CloseCase::NonRun);
            bind(&mut daemon.stream, &daemon.root1, 1, run).await;
            if matches!(case, CloseCase::Duplicate) {
                bind(&mut daemon.stream, &daemon.root1, 2, true).await;
            }
            #[cfg(feature = "test-timing-hooks")]
            let registration = if matches!(case, CloseCase::Duplicate) {
                let gate = daemon.root1.join("registration.gate");
                let sampled = daemon.root1.join("goodbye-sampled");
                std::fs::write(&gate, "hold").unwrap();
                let gate_env =
                    set_test_env("AFT_TEST_BASH_REGISTRATION_GATE", gate.to_str().unwrap());
                let sampled_env = set_test_env(
                    "AFT_TEST_SCOPE_GOODBYE_SAMPLED_FILE",
                    sampled.to_str().unwrap(),
                );
                Some((gate, sampled, gate_env, sampled_env))
            } else {
                None
            };
            #[cfg(feature = "test-timing-hooks")]
            let classic_startup = if matches!(case, CloseCase::NonRun) {
                let gate = daemon.root1.join("after-create.gate");
                let sampled = daemon.root1.join("goodbye-sampled");
                std::fs::write(&gate, "hold").unwrap();
                let gate_env =
                    set_test_env("AFT_TEST_BASH_AFTER_CREATE_GATE", gate.to_str().unwrap());
                let sampled_env = set_test_env(
                    "AFT_TEST_SCOPE_GOODBYE_SAMPLED_FILE",
                    sampled.to_str().unwrap(),
                );
                Some((GateCleanup(gate), sampled, gate_env, sampled_env))
            } else {
                None
            };
            let pid_path = daemon.root1.join("grandchild.pid");
            let command = format!(
                "(trap '' TERM; sleep 30) & echo $! > '{}'; wait",
                pid_path.display()
            );
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":command,"timeout":20000,"wait":true}),
            )
            .await;
            if run {
                let request = describe_phase(&mut daemon.stream, "pre-bash Live check").await;
                // The daemon confirms the same live epoch before bash starts;
                // the bind's deadline remains the upper bound on its timeout.
                answer(
                    &mut daemon.stream,
                    &request,
                    ScopeStatus::Live,
                    true,
                    Some(1),
                )
                .await;
            }
            #[cfg(feature = "test-timing-hooks")]
            if let Some((gate, _, _, _)) = &registration {
                // Registration precedes bootstrap release, so the command cannot
                // publish its PID until this fixture lets registration finish.
                wait_test_marker(&gate.with_extension("reached")).await;
                assert!(
                    !pid_path.exists(),
                    "bash command ran before task registration"
                );
                std::fs::remove_file(gate).unwrap();
            }
            #[cfg(feature = "test-timing-hooks")]
            if let Some((gate, _, _, _)) = &classic_startup {
                // This checkpoint follows bootstrap release, so observing the
                // command's PID is safe even while spawn dispatch is paused.
                wait_test_marker(&gate.0.with_extension("held")).await;
            }
            let pid = child_pid(&pid_path).await;
            let _cleanup = ChildCleanup(pid);
            assert!(
                alive(pid),
                "control: grandchild must be live before closing the scope"
            );
            if matches!(case, CloseCase::Duplicate) {
                // The command can start before its task joins scope monitoring.
                // Wait for spawn dispatch to finish so closing the sibling route
                // finds the task and asks whether its scope is still live.
                assert_run_not_killing(&pid_path).await;
            }
            // Close the unused sibling first: it must already share the run's
            // lifetime state even though it has never made a tool call.
            let first_close = if matches!(case, CloseCase::Duplicate) {
                2
            } else {
                1
            };
            send_route_goodbye(&mut daemon.stream, first_close, 200).await;
            #[cfg(feature = "test-timing-hooks")]
            if let Some((gate, sampled, _, _)) = &classic_startup {
                // Goodbye must see the started command before spawn dispatch
                // returns; the regular 60 s poll cannot rescue a missed close.
                wait_test_marker(sampled).await;
                assert_eq!(std::fs::read_to_string(sampled).unwrap(), "1");
                // Revocation runs on the same actor as spawn dispatch.
                std::fs::remove_file(&gate.0).unwrap();
            }
            #[cfg(feature = "test-timing-hooks")]
            if let Some((_, sampled, _, _)) = &registration {
                wait_test_marker(sampled).await;
                assert_eq!(std::fs::read_to_string(sampled).unwrap(), "1");
            }
            {
                // Scope end governs classic and codemode tasks alike.
                let request = describe_phase(&mut daemon.stream, "first-close describe").await;
                assert!(alive(pid), "route close alone must not kill");
                if matches!(case, CloseCase::Duplicate) {
                    #[cfg(feature = "test-timing-hooks")]
                    if let Some((_, sampled, _, _)) = &registration {
                        std::fs::remove_file(sampled).unwrap();
                    }
                    send_route_goodbye(&mut daemon.stream, 1, 201).await;
                    #[cfg(feature = "test-timing-hooks")]
                    if let Some((_, sampled, _, _)) = &registration {
                        wait_test_marker(sampled).await;
                        assert_eq!(std::fs::read_to_string(sampled).unwrap(), "1");
                    }
                    no_extra_frames_until_pong(
                        &mut daemon.stream,
                        9003,
                        "duplicate closes must share one in-flight describe",
                    )
                    .await;
                }
                match case {
                    CloseCase::NotSynced => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::NotLive,
                            false,
                            None,
                        )
                        .await;
                        assert_hold_or_retry(&mut daemon.stream, pid, &pid_path, "not-synced")
                            .await;
                    }
                    #[cfg(feature = "test-timing-hooks")]
                    CloseCase::SlowResync => {
                        let mut request = request;
                        for attempt in 0..3 {
                            answer(
                                &mut daemon.stream,
                                &request,
                                ScopeStatus::NotLive,
                                false,
                                None,
                            )
                            .await;
                            request = read_frame_within(
                                &mut daemon.stream,
                                Duration::from_secs(5),
                                "confirmation after an inconclusive reply",
                            )
                            .await
                            .expect("scope confirmation stopped after three inconclusive replies");
                            assert_eq!(request.header.ty, FrameType::Request);
                            assert_eq!(request.header.channel, 0);
                            let body: Value = serde_json::from_slice(&request.body).unwrap();
                            assert_eq!(body["op"], "scope.describe");
                            assert!(alive(pid), "inconclusive reply {attempt} must hold work");
                            assert_run_not_killing(&pid_path).await;
                        }
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await;
                    }
                    CloseCase::Error => {
                        send_frame(
                            &mut daemon.stream,
                            Frame::build(
                                FrameType::Error,
                                control_flags(),
                                0,
                                0,
                                request.header.corr,
                                serde_json::to_vec(&subc_protocol::ErrorBody::new(
                                    "unavailable",
                                    "not ready",
                                ))
                                .unwrap(),
                            )
                            .unwrap(),
                        )
                        .await;
                        assert_hold_or_retry(&mut daemon.stream, pid, &pid_path, "describe error")
                            .await;
                    }
                    CloseCase::Timeout => {
                        #[cfg(not(feature = "test-timing-hooks"))]
                        assert!(read_frame_within(
                            &mut daemon.stream,
                            Duration::from_secs(11),
                            "production describe timeout"
                        )
                        .await
                        .is_none());
                        assert_hold_or_retry(
                            &mut daemon.stream,
                            pid,
                            &pid_path,
                            "describe timeout",
                        )
                        .await;
                    }
                    CloseCase::NewEpoch => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::Live,
                            true,
                            Some(2),
                        )
                        .await;
                        // Goodbye removed the task's route without ending its
                        // scope. Later scope.describe requests are valid while
                        // it runs. The paused-clock worker test checks cadence;
                        // until our Ping receives a Pong, reject all other frames.
                        send_frame(
                            &mut daemon.stream,
                            Frame::build(FrameType::Ping, control_flags(), 0, 0, 9002, Vec::new())
                                .unwrap(),
                        )
                        .await;
                        loop {
                            let frame =
                                read_frame_timeout(&mut daemon.stream, "live epoch barrier").await;
                            if frame.header.ty == FrameType::Pong && frame.header.corr == 9002 {
                                break;
                            }
                            assert_eq!(
                                frame.header.ty,
                                FrameType::Request,
                                "unexpected frame after live epoch move: {:?}",
                                frame
                            );
                            assert_eq!(frame.header.channel, 0);
                            let body: Value = serde_json::from_slice(&frame.body).unwrap();
                            assert_eq!(body["op"], "scope.describe", "{body}");
                            assert_eq!(body["owner"], serde_json::to_value(stamp().owner).unwrap());
                            assert_eq!(body["ref"], "run-scope");
                            answer(&mut daemon.stream, &frame, ScopeStatus::Live, true, Some(2))
                                .await;
                        }
                        assert!(alive(pid), "a live epoch move must not kill");
                        assert_run_not_killing(&pid_path).await;
                    }
                    CloseCase::NotLive => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::NotLive,
                            true,
                            None,
                        )
                        .await
                    }
                    _ => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await
                    }
                }
                let should_kill = !matches!(case, CloseCase::NewEpoch)
                    && (cfg!(feature = "test-timing-hooks")
                        || !matches!(
                            case,
                            CloseCase::NotSynced | CloseCase::Error | CloseCase::Timeout
                        ));
                if should_kill {
                    wait_dead(pid).await;
                    if run {
                        assert_run_kill_settled(&pid_path, "run-session").await;
                    } else {
                        assert_scope_kill_settled(&pid_path).await;
                    }
                }
                if matches!(case, CloseCase::Duplicate) {
                    no_extra_frames_until_pong(
                        &mut daemon.stream,
                        9004,
                        "duplicate closes must not queue another describe after settlement",
                    )
                    .await;
                    assert!(
                        read_frame_within(
                            &mut daemon.stream,
                            Duration::from_millis(300),
                            "queued duplicate describe"
                        )
                        .await
                        .is_none(),
                        "duplicate closes must not queue another describe after the first finishes"
                    );
                }
            }
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        observing_dispatch,
    );
}

#[cfg(unix)]
#[test]
fn scope_revocation_ended_kills_grandchild_group() {
    close_case("scope-ended-group", CloseCase::Ended);
}
#[cfg(unix)]
#[test]
fn scope_revocation_live_new_epoch_holds_group() {
    close_case("scope-new-epoch", CloseCase::NewEpoch);
}
#[cfg(unix)]
#[test]
fn scope_revocation_not_live_synced_kills_group() {
    close_case("scope-not-live", CloseCase::NotLive);
}
#[cfg(unix)]
#[test]
fn scope_revocation_not_synced_holds_group() {
    close_case("scope-not-synced", CloseCase::NotSynced);
}
#[cfg(unix)]
#[test]
fn scope_revocation_describe_error_holds_group() {
    close_case("scope-describe-error", CloseCase::Error);
}
#[cfg(unix)]
#[test]
fn scope_revocation_describe_timeout_holds_group() {
    close_case("scope-describe-timeout", CloseCase::Timeout);
}
#[cfg(unix)]
#[test]
fn scope_revocation_duplicate_closes_share_describe() {
    close_case("scope-duplicate-close", CloseCase::Duplicate);
}
#[cfg(unix)]
#[test]
fn scope_revocation_classic_route_ended_kills_group() {
    close_case("scope-non-run-close", CloseCase::NonRun);
}

#[test]
fn scope_revocation_run_pty_refused_before_spawn() {
    run_subc_bridge_test_with_dispatch(
        "run-pty-refused",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            let marker = daemon.root1.join("must-not-exist");
            let mut args = json!({"command":format!("touch '{}'", marker.display())});
            args["pty"] = json!(true);
            call(&mut daemon.stream, 1, 100, args).await;
            let response = read_frame_timeout(&mut daemon.stream, "named PTY refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, "run_pty_refused");
            assert!(error.message.contains("codemode runs can't open a PTY"));
            assert!(!marker.exists());
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_unreadable_run_record_refused_before_spawn() {
    run_subc_bridge_test_with_dispatch(
        "run-record-refused",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            let marker = daemon.root1.join("must-not-exist");
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":format!("touch '{}'", marker.display())}),
            )
            .await;
            let request = describe(&mut daemon.stream).await;
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Error,
                    control_flags(),
                    0,
                    0,
                    request.header.corr,
                    serde_json::to_vec(&ErrorBody::new("unavailable", "cannot read record"))
                        .unwrap(),
                )
                .unwrap(),
            )
            .await;
            let response =
                read_frame_timeout(&mut daemon.stream, "unreadable record refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, "run_scope_unavailable");
            assert!(!marker.exists());
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_codemode_watch_not_served() {
    run_subc_bridge_test_with_dispatch(
        "run-watch-background",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            send_frame(&mut daemon.stream, Frame::build(FrameType::Request, control_flags(), 1, 1, 100,
            serde_json::to_vec(&json!({"name":"bash_watch", "preset":"worker", "arguments":{"taskId":"absent", "background":true}})).unwrap()).unwrap()).await;
            let response = read_frame_timeout(&mut daemon.stream, "watch background refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(
                error.code,
                cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_non_run_background_is_unchanged() {
    run_subc_bridge_test_with_dispatch(
        "non-run-background",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, false).await;
            let marker = daemon.root1.join("background-ran");
            #[cfg(windows)]
            let command = format!(
                "Start-Sleep -Milliseconds 50; Set-Content -LiteralPath '{}' -Value done",
                marker.display().to_string().replace('\'', "''")
            );
            #[cfg(not(windows))]
            let command = format!("sleep 0.05; printf done > '{}'", marker.display());
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":command,"background":true}),
            )
            .await;
            let response = read_frame_timeout(&mut daemon.stream, "normal background launch").await;
            assert_eq!(response.header.ty, FrameType::Response);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(
                    Instant::now() < deadline,
                    "non-run background command did not run"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_record_timeout_refuses_without_blocking_frames() {
    run_subc_bridge_test_with_dispatch(
        "run-record-timeout",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            let marker = daemon.root1.join("must-not-exist");
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":format!("touch '{}'", marker.display())}),
            )
            .await;
            let _request = describe(&mut daemon.stream).await;
            send_frame(
                &mut daemon.stream,
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 500, Vec::new()).unwrap(),
            )
            .await;
            let pong = read_frame_within(
                &mut daemon.stream,
                Duration::from_secs(1),
                "pong while scope read waits",
            )
            .await
            .expect("scope read blocked the frame loop");
            assert_eq!(pong.header.ty, FrameType::Pong);
            let response = read_frame_timeout(&mut daemon.stream, "record timeout refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, "run_scope_unavailable");
            assert!(!marker.exists());
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[cfg(all(unix, feature = "test-timing-hooks"))]
#[test]
fn scope_revocation_three_inconclusive_then_ended_kills_group() {
    close_case("scope-slow-owner-resync", CloseCase::SlowResync);
}

fn reply_json(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}

fn reply_text(frame: &Frame) -> String {
    reply_json(frame)["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A tool response's native error code, from `structuredContent`.
fn reply_code(frame: &Frame) -> String {
    reply_json(frame)["structuredContent"]["code"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

async fn expect_refusal(stream: &mut tokio::net::TcpStream, label: &str) -> ErrorBody {
    let reply = read_frame_timeout(stream, label).await;
    assert_eq!(
        reply.header.ty,
        FrameType::Error,
        "{label}: {}",
        String::from_utf8_lossy(&reply.body)
    );
    serde_json::from_slice(&reply.body).unwrap()
}

/// Send a provider operation (`late_results`, `tool.withdraw`) on `channel`.
async fn provider_op(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64, body: Value) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            1,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

/// Bind a run route on `channel` with `scope`, in its own session and root.
async fn bind_run(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    scope: subc_protocol::scope::ScopeStamp,
) {
    bind_mode_as(
        stream,
        root,
        channel,
        Some(subc_protocol::call_mode::CallMode::Codemode),
        Some(scope),
        "program-session",
    )
    .await;
}

#[test]
fn codemode_run_owner_not_bound_without_matching_live_head_route() {
    run_subc_bridge_test_with_dispatch(
        "codemode-owner-not-bound",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            std::fs::write(daemon.root1.join("owned.txt"), "head-owned").unwrap();
            let read = json!({"path": daemon.root1.join("owned.txt")});
            bind_run(&mut daemon.stream, &daemon.root2, 1, stamp()).await;
            // No head route is bound at all.
            named_call(&mut daemon.stream, 1, 100, "read", read.clone()).await;
            let error = expect_refusal(&mut daemon.stream, "no head route").await;
            assert_eq!(error.code, "run_owner_not_bound");
            assert_eq!(
                error.detail.as_ref().unwrap()["reason"],
                "head_route_not_bound"
            );
            // A run on another agent is not served by this head's route.
            bind_head(&mut daemon.stream, &daemon.root1, 3, "head-session").await;
            let mut other_agent = stamp();
            other_agent.scope_ref = "run-scope-z".into();
            other_agent.attributes.agent_id = Some("agent-z".into());
            other_agent.attributes.run_id = Some("run-z".into());
            bind_run(&mut daemon.stream, &daemon.root2, 2, other_agent).await;
            named_call(&mut daemon.stream, 2, 101, "read", read.clone()).await;
            let error = expect_refusal(&mut daemon.stream, "mismatched agent").await;
            assert_eq!(error.code, "run_owner_not_bound");
            assert_eq!(error.detail.as_ref().unwrap()["reason"], "agent_mismatch");
            // A run whose parent link has ended serves no new call even
            // though a matching head route is live.
            let mut ended = stamp();
            ended.scope_ref = "run-scope-ended".into();
            ended.parent_state = Some(subc_protocol::scope::ParentState::Ended);
            bind_run(&mut daemon.stream, &daemon.root2, 4, ended).await;
            named_call(&mut daemon.stream, 4, 102, "read", read.clone()).await;
            let error = expect_refusal(&mut daemon.stream, "ended parent").await;
            assert_eq!(error.code, "run_owner_not_bound");
            assert_eq!(error.detail.as_ref().unwrap()["reason"], "parent_ended");
            // Control: the linked run with its agent's head route is served.
            named_call(&mut daemon.stream, 1, 103, "read", read).await;
            let reply = read_frame_timeout(&mut daemon.stream, "owned read").await;
            assert_eq!(reply.header.ty, FrameType::Response);
            assert!(reply_text(&reply).contains("head-owned"));
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

/// Runs the real `configure`, so the project and user config files decide the
/// head's policy; every other command goes to the shared bridge stand-in.
fn configured_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    if req.command == "configure" {
        return aft::commands::configure::handle_configure(&req, ctx);
    }
    bridge_dispatch(req, ctx)
}

/// Project config with the index builders off, merged with `extra`.
fn write_project_config(root: &Path, extra: Value) {
    let mut doc = json!({
        "bash": {"foreground_wait_window_ms": 10},
        "indexes": {"trigram": false, "semantic": false, "callgraph": false},
    });
    for (key, value) in extra.as_object().unwrap() {
        match (doc.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(add)) => {
                existing.extend(add.clone());
            }
            _ => {
                doc[key] = value.clone();
            }
        }
    }
    std::fs::create_dir_all(root.join(".cortexkit")).unwrap();
    std::fs::write(
        root.join(".cortexkit/aft.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
}

#[test]
fn codemode_path_outside_head_root_or_denied_by_head_policy_is_refused() {
    run_subc_bridge_test_with_dispatch(
        "codemode-head-policy",
        Duration::from_secs(30),
        |input| async move {
            // The head restricts itself to its root and denies one path in it.
            std::fs::write(
                &input.user_config_path,
                serde_json::to_vec(&json!({"restrict_to_project_root": true})).unwrap(),
            )
            .unwrap();
            let mut daemon = open_scope_daemon_session(input).await;
            let secret = daemon.root1.join("secret.txt");
            write_project_config(&daemon.root1, json!({"sandbox": {"read_deny": [&secret]}}));
            write_project_config(&daemon.root2, json!({}));
            std::fs::write(daemon.root1.join("inside.txt"), "inside-head-root").unwrap();
            std::fs::write(&secret, "denied-by-head-policy").unwrap();
            std::fs::write(daemon.root2.join("program.txt"), "program-root-file").unwrap();
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            // The run's own bind names root2; that is configuration, not authority.
            bind_run(&mut daemon.stream, &daemon.root2, 2, stamp()).await;
            named_call(
                &mut daemon.stream,
                2,
                100,
                "read",
                json!({"path": daemon.root1.join("inside.txt")}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "inside read").await;
            assert!(
                reply_text(&reply).contains("inside-head-root"),
                "{}",
                reply_text(&reply)
            );
            named_call(
                &mut daemon.stream,
                2,
                101,
                "read",
                json!({"path": daemon.root2.join("program.txt")}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "outside head root").await;
            assert!(
                !reply_text(&reply).contains("program-root-file"),
                "{}",
                reply_text(&reply)
            );
            assert_eq!(
                reply_code(&reply),
                "path_outside_root",
                "{}",
                reply_text(&reply)
            );
            named_call(&mut daemon.stream, 2, 102, "read", json!({"path": &secret})).await;
            let reply = read_frame_timeout(&mut daemon.stream, "denied path").await;
            assert!(
                !reply_text(&reply).contains("denied-by-head-policy"),
                "{}",
                reply_text(&reply)
            );
            assert_eq!(reply_code(&reply), "denied_floor", "{}", reply_text(&reply));
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        configured_dispatch,
    );
}

#[test]
fn codemode_call_needing_an_ask_is_refused_permission_ask_unavailable() {
    run_subc_bridge_test_with_dispatch(
        "codemode-ask-refused",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            write_project_config(&daemon.root1, json!({}));
            write_project_config(&daemon.root2, json!({}));
            std::fs::write(daemon.root2.join("outside.txt"), "needs-an-ask").unwrap();
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            // The head chose ask mode, so a path outside its root asks.
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Request,
                    control_flags(),
                    1,
                    1,
                    99,
                    serde_json::to_vec(
                        &json!({"op":"tool.catalog", "preset":"head", "params":{"mode":"ask"}}),
                    )
                    .unwrap(),
                )
                .unwrap(),
            )
            .await;
            let catalog = read_frame_timeout(&mut daemon.stream, "head catalog").await;
            assert_eq!(
                catalog.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&catalog.body)
            );
            bind_run(&mut daemon.stream, &daemon.root2, 2, stamp()).await;
            let outside = daemon.root2.join("outside.txt");
            named_call(
                &mut daemon.stream,
                2,
                100,
                "read",
                json!({"path": &outside}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "ask refusal").await;
            assert!(
                !reply_text(&reply).contains("needs-an-ask"),
                "{}",
                reply_text(&reply)
            );
            assert_eq!(
                reply_code(&reply),
                "permission_ask_unavailable",
                "{}",
                reply_text(&reply)
            );
            let data = &reply_json(&reply)["structuredContent"];
            assert_eq!(data["permission"], "read", "{data}");
            assert!(
                data["resource"].as_str().unwrap().ends_with("outside.txt"),
                "{data}"
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        configured_dispatch,
    );
}

#[cfg(unix)]
#[test]
fn codemode_background_task_killed_when_run_scope_ends_and_only_then() {
    run_subc_bridge_test_with_dispatch(
        "codemode-background-run-end",
        Duration::from_secs(30),
        |input| async move {
            use subc_protocol::scope::ScopeStatus;
            let mut daemon = open_scope_daemon_session(input).await;
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            // Two routes of one run: closing one is a transient close.
            bind_run(&mut daemon.stream, &daemon.root2, 2, stamp()).await;
            bind_run(&mut daemon.stream, &daemon.root2, 3, stamp()).await;
            let pid_path = daemon.root1.join("background.pid");
            let command = format!(
                "(trap '' TERM; sleep 30) & echo $! > '{}'; wait",
                pid_path.display()
            );
            call(
                &mut daemon.stream,
                2,
                100,
                json!({"command":command, "background":true}),
            )
            .await;
            let request = describe_phase(&mut daemon.stream, "pre-bash Live check").await;
            answer(
                &mut daemon.stream,
                &request,
                ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            let launch = read_frame_timeout(&mut daemon.stream, "background launch").await;
            assert_eq!(
                launch.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&launch.body)
            );
            let pid = child_pid(&pid_path).await;
            let _cleanup = ChildCleanup(pid);
            let (registry, task) = observed_task(&pid_path).await;
            // The task belongs to the head's session, so the agent reads it.
            assert!(registry
                .status(&task, "head-session", None, None, 0)
                .is_some());
            assert!(registry
                .status(&task, "program-session", None, None, 0)
                .is_none());
            named_call(
                &mut daemon.stream,
                1,
                101,
                "bash_status",
                json!({"taskId": &task}),
            )
            .await;
            let status = read_frame_timeout(&mut daemon.stream, "head bash_status").await;
            assert!(
                reply_text(&status).contains(&task),
                "{}",
                reply_text(&status)
            );
            assert!(
                !reply_text(&status).contains("not found"),
                "{}",
                reply_text(&status)
            );
            // A transient close without a definitive describe keeps it running.
            send_route_goodbye(&mut daemon.stream, 2, 200).await;
            let request = describe_phase(&mut daemon.stream, "transient close describe").await;
            answer(
                &mut daemon.stream,
                &request,
                ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            no_extra_frames_until_pong(&mut daemon.stream, 9100, "live run keeps its task").await;
            assert!(
                alive(pid),
                "a transient close must not kill the program's task"
            );
            let snapshot = registry
                .status(&task, "head-session", None, None, 0)
                .unwrap();
            assert_eq!(snapshot.info.status, BgTaskStatus::Running);
            // The run's end, confirmed by describe, kills it.
            send_route_goodbye(&mut daemon.stream, 3, 201).await;
            let request = describe_phase(&mut daemon.stream, "run end describe").await;
            answer(
                &mut daemon.stream,
                &request,
                ScopeStatus::Ended,
                true,
                Some(1),
            )
            .await;
            wait_dead(pid).await;
            assert_run_kill_settled(&pid_path, "head-session").await;
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        observing_dispatch,
    );
}

#[test]
fn codemode_edit_lands_in_head_undo_history_and_undoes() {
    run_subc_bridge_test_with_dispatch(
        "codemode-edit-undo",
        Duration::from_secs(30),
        |input| async move {
            // Fixture roots live under the system temp directory, where undo
            // snapshots are otherwise skipped.
            let _backups = set_test_env("AFT_TEST_ALLOW_TEMP_BACKUPS", "1");
            let mut daemon = open_scope_daemon_session(input).await;
            let file = daemon.root1.join("edited.txt");
            std::fs::write(&file, "before\n").unwrap();
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            bind_run(&mut daemon.stream, &daemon.root2, 2, stamp()).await;
            named_call(
                &mut daemon.stream,
                2,
                100,
                "write",
                json!({"path": &file, "content": "after\n"}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "program write").await;
            assert_eq!(reply.header.ty, FrameType::Response);
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "after\n",
                "{}",
                reply_text(&reply)
            );
            // The head agent sees the program's edit in its own history.
            named_call(
                &mut daemon.stream,
                1,
                101,
                "safety",
                json!({"op":"history", "path": &file}),
            )
            .await;
            let history = read_frame_timeout(&mut daemon.stream, "head history").await;
            assert!(
                reply_text(&history).contains("[codemode run run-a]"),
                "{}",
                String::from_utf8_lossy(&history.body)
            );
            named_call(
                &mut daemon.stream,
                1,
                102,
                "safety",
                json!({"op":"undo", "path": &file}),
            )
            .await;
            let undo = read_frame_timeout(&mut daemon.stream, "head undo").await;
            assert_eq!(undo.header.ty, FrameType::Response);
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "before\n",
                "{}",
                reply_text(&undo)
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[cfg(unix)]
#[test]
fn codemode_bash_runs_in_a_real_shell_without_rewrites() {
    run_subc_bridge_test_with_dispatch(
        "codemode-no-rewrite",
        Duration::from_secs(30),
        |input| async move {
            use subc_protocol::scope::ScopeStatus;
            let mut daemon = open_scope_daemon_session(input).await;
            std::fs::write(daemon.root1.join("cat.txt"), "alpha\nbeta\n").unwrap();
            write_project_config(&daemon.root1, json!({"bash": {"rewrite": true}}));
            write_project_config(&daemon.root2, json!({}));
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            bind_run(&mut daemon.stream, &daemon.root2, 2, stamp()).await;
            // Control: the head's direct call is answered by the read rewrite.
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":"cat cat.txt","timeout":5000}),
            )
            .await;
            let head = read_frame_timeout(&mut daemon.stream, "head cat").await;
            assert!(reply_text(&head).contains("alpha"), "{}", reply_text(&head));
            let head_route =
                aft::bash_rewrite::dispatch::route_record("subc-1-100").map(|record| record.route);
            assert!(
                matches!(
                    head_route,
                    Some(aft::bash_rewrite::dispatch::DispatchRoute::Rewritten { .. })
                ),
                "the head's cat was not rewritten: {head_route:?}"
            );
            call(
                &mut daemon.stream,
                2,
                101,
                json!({"command":"cat cat.txt","timeout":5000}),
            )
            .await;
            let request = describe_phase(&mut daemon.stream, "pre-bash Live check").await;
            answer(
                &mut daemon.stream,
                &request,
                ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            let program = read_frame_timeout(&mut daemon.stream, "program cat").await;
            let program_text = reply_text(&program);
            assert!(program_text.contains("alpha\nbeta"), "{program_text}");
            assert!(
                !matches!(
                    aft::bash_rewrite::dispatch::route_record("subc-2-101")
                        .map(|record| record.route),
                    Some(aft::bash_rewrite::dispatch::DispatchRoute::Rewritten { .. })
                ),
                "a program's cat must run in a real shell"
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        configured_dispatch,
    );
}

#[test]
fn codemode_call_runs_with_head_principal_and_run_route_finds_its_ledger_row() {
    // The call-ledger warning explains why an outcome_unknown result could not
    // be confirmed as durably recorded.
    let _ = env_logger::Builder::new()
        .filter_module("aft::subc::tool_provider", log::LevelFilter::Warn)
        .is_test(true)
        .try_init();
    run_subc_bridge_test_with_dispatch(
        "codemode-ledger-carrier",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            write_project_config(&daemon.root1, json!({}));
            write_project_config(&daemon.root2, json!({}));
            std::fs::write(daemon.root1.join("ledger.txt"), "ledger-read").unwrap();
            // Ledger keys outlive one module run, so each run uses fresh ones.
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let read_key = format!("program-read-{unique}");
            let shell_key = format!("program-shell-{unique}");
            bind_route_as(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(subc_protocol::call_mode::CallMode::Classic),
                Some(head_stamp("agent-a")),
                "head-session",
                Principal::Direct,
                true,
            )
            .await;
            // The program's carrier binds the run route under its own principal.
            bind_route_as(
                &mut daemon.stream,
                &daemon.root2,
                2,
                Some(subc_protocol::call_mode::CallMode::Codemode),
                Some(stamp()),
                "program-session",
                Principal::Reserved {
                    module_id: "basal".into(),
                },
                true,
            )
            .await;
            // Bind acknowledgement precedes the asynchronous aft.db open.
            let until = Instant::now() + Duration::from_secs(15);
            let mut corr = 300;
            loop {
                corr += 1;
                provider_op(
                    &mut daemon.stream,
                    2,
                    corr,
                    json!({"name": "late_results", "arguments": {}}),
                )
                .await;
                let ready = read_frame_timeout(&mut daemon.stream, "ledger readiness").await;
                if ready.header.ty == FrameType::Response {
                    break;
                }
                assert_eq!(
                    reply_json(&ready)["code"],
                    "database_initializing",
                    "{}",
                    String::from_utf8_lossy(&ready.body)
                );
                assert!(Instant::now() < until, "aft.db never opened");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Both contexts share App.db. Verify that configuration and the
            // opened database stay in this fixture's private storage instead
            // of using the process-global default data directory.
            let storage = std::path::PathBuf::from(
                std::env::var_os("AFT_STORAGE_DIR").expect("fixture storage selection"),
            )
            .canonicalize()
            .unwrap();
            for project in [&daemon.root1, &daemon.root2] {
                let ctx = daemon
                    .executor
                    .actor_context(&ProjectRootId::from_path(project).unwrap())
                    .expect("bound actor");
                let actor_storage = ctx
                    .config()
                    .storage_dir
                    .clone()
                    .expect("configured storage");
                assert_eq!(actor_storage.canonicalize().unwrap(), storage);
                let db = ctx.db().expect("ready database");
                let database: String = db
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT file FROM pragma_database_list WHERE name='main'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    std::path::PathBuf::from(database).canonicalize().unwrap(),
                    storage.join("aft.db")
                );
            }
            let keyed = json!({
                "name": "glob",
                "preset": "head",
                "call_key": &read_key,
                "arguments": {"pattern": "ledger*.txt", "path": &daemon.root1},
            });
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Request,
                    control_flags(),
                    2,
                    1,
                    400,
                    serde_json::to_vec(&keyed).unwrap(),
                )
                .unwrap(),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "keyed program read").await;
            assert!(
                reply_text(&reply).contains("ledger.txt"),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            // The run route answers for the call it sent: its withdraw lookup,
            // keyed by the same carrier late_results uses, finds the call's
            // retained result.
            provider_op(
                &mut daemon.stream,
                2,
                401,
                json!({"name": "tool.withdraw", "arguments": {"call_key": &read_key}}),
            )
            .await;
            let withdrawn = read_frame_timeout(&mut daemon.stream, "withdraw lookup").await;
            assert_eq!(
                withdrawn.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&withdrawn.body)
            );
            let withdrawn = reply_json(&withdrawn);
            assert_eq!(withdrawn["answer"], "completed", "{withdrawn}");
            assert!(
                withdrawn["result"].to_string().contains("ledger.txt"),
                "{withdrawn}"
            );
            // A keyed shell call records its ledger row and its task under the
            // run route's carrier too.
            let keyed_shell = json!({
                "name": "bash",
                "preset": "head",
                "call_key": &shell_key,
                "arguments": {"command": "echo ledger-shell", "timeout": 5000},
            });
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Request,
                    control_flags(),
                    2,
                    1,
                    402,
                    serde_json::to_vec(&keyed_shell).unwrap(),
                )
                .unwrap(),
            )
            .await;
            let request = describe_phase(&mut daemon.stream, "pre-bash Live check").await;
            answer(
                &mut daemon.stream,
                &request,
                subc_protocol::scope::ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            let shell = read_frame_timeout(&mut daemon.stream, "keyed program shell").await;
            assert!(
                reply_text(&shell).contains("ledger-shell"),
                "{}",
                String::from_utf8_lossy(&shell.body)
            );
            provider_op(
                &mut daemon.stream,
                2,
                403,
                json!({"name": "tool.withdraw", "arguments": {"call_key": &shell_key}}),
            )
            .await;
            let withdrawn =
                reply_json(&read_frame_timeout(&mut daemon.stream, "shell withdraw lookup").await);
            assert_eq!(withdrawn["answer"], "completed", "{withdrawn}");
            assert!(
                withdrawn["result"].to_string().contains("ledger-shell"),
                "{withdrawn}"
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        configured_dispatch,
    );
}

/// Wait until the route's `aft.db` is open: the bind acknowledgement comes
/// before the asynchronous database open, and ledger operations refuse with
/// `database_initializing` until then.
async fn wait_for_ledger(stream: &mut tokio::net::TcpStream, channel: u16, first_corr: u64) {
    let until = Instant::now() + Duration::from_secs(15);
    let mut corr = first_corr;
    loop {
        provider_op(
            stream,
            channel,
            corr,
            json!({"name": "late_results", "arguments": {}}),
        )
        .await;
        let ready = read_frame_timeout(stream, "ledger readiness").await;
        if ready.header.ty == FrameType::Response {
            return;
        }
        assert_eq!(
            reply_json(&ready)["code"],
            "database_initializing",
            "{}",
            String::from_utf8_lossy(&ready.body)
        );
        assert!(Instant::now() < until, "aft.db never opened");
        corr += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A codemode run bound to another root than its head runs its shell calls on
/// the head's root, so their tasks live in the head root's registry. A
/// `tool.withdraw` on the run route must still stop such a call: the kill goes
/// to the registry that owns the task, not to the run route's own root.
#[cfg(unix)]
#[test]
fn codemode_withdraw_kills_a_running_shell_call_on_the_head_root() {
    run_subc_bridge_test_with_dispatch(
        "codemode-withdraw-head-root",
        Duration::from_secs(40),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            write_project_config(&daemon.root1, json!({}));
            write_project_config(&daemon.root2, json!({}));
            // Ledger keys outlive one module run, so each run uses a fresh one.
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let shell_key = format!("program-withdrawn-shell-{unique}");
            bind_route_as(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(subc_protocol::call_mode::CallMode::Classic),
                Some(head_stamp("agent-a")),
                "head-session",
                Principal::Direct,
                true,
            )
            .await;
            bind_route_as(
                &mut daemon.stream,
                &daemon.root2,
                2,
                Some(subc_protocol::call_mode::CallMode::Codemode),
                Some(stamp()),
                "program-session",
                Principal::Reserved {
                    module_id: "basal".into(),
                },
                true,
            )
            .await;
            wait_for_ledger(&mut daemon.stream, 2, 300).await;
            let pid_path = daemon.root1.join("withdrawn.pid");
            let command = format!("sleep 30 & echo $! > '{}'; wait", pid_path.display());
            let keyed_shell = json!({
                "name": "bash",
                "preset": "head",
                "call_key": &shell_key,
                "arguments": {"command": command, "timeout": 30000},
            });
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Request,
                    control_flags(),
                    2,
                    1,
                    400,
                    serde_json::to_vec(&keyed_shell).unwrap(),
                )
                .unwrap(),
            )
            .await;
            let request = describe_phase(&mut daemon.stream, "pre-bash Live check").await;
            answer(
                &mut daemon.stream,
                &request,
                subc_protocol::scope::ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            let pid = child_pid(&pid_path).await;
            let _cleanup = ChildCleanup(pid);
            assert!(alive(pid), "the shell call must be running before withdraw");

            provider_op(
                &mut daemon.stream,
                2,
                401,
                json!({"name": "tool.withdraw", "arguments": {"call_key": &shell_key}}),
            )
            .await;
            // The killed call's own reply may arrive before the withdraw answer.
            let mut call_ended = false;
            let withdrawn = loop {
                let frame = read_frame_timeout(&mut daemon.stream, "withdraw answer").await;
                match frame.header.corr {
                    401 => break frame,
                    400 => call_ended = true,
                    corr => panic!(
                        "unexpected frame corr={corr}: {}",
                        String::from_utf8_lossy(&frame.body)
                    ),
                }
            };
            assert_eq!(
                withdrawn.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&withdrawn.body)
            );
            assert_eq!(
                reply_json(&withdrawn),
                json!({"answer": "already_started", "outcome": "unknown"})
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive(pid) {
                assert!(
                    Instant::now() < deadline,
                    "tool.withdraw on the run route left the head-root shell call running"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if !call_ended {
                let ended = read_frame_timeout(&mut daemon.stream, "withdrawn call end").await;
                assert_eq!(ended.header.corr, 400);
            }
            // The answer is frozen once the kill landed: a repeat returns it.
            provider_op(
                &mut daemon.stream,
                2,
                402,
                json!({"name": "tool.withdraw", "arguments": {"call_key": &shell_key}}),
            )
            .await;
            let repeat = read_frame_timeout(&mut daemon.stream, "repeated withdraw").await;
            assert_eq!(repeat.body, withdrawn.body);
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        configured_dispatch,
    );
}

/// A worker's own task scope, under the agent-a head scope.
fn worker_stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind":"reserved", "module_id":PREFRONTAL},
        "ref":"worker-scope", "scope_epoch":1, "kind":"worker",
        "parent": {"owner": {"kind":"reserved", "module_id":PREFRONTAL}, "ref":"head-scope", "scope_epoch":1},
        "parent_state":"linked",
        "owner_authorized":true
    }))
    .unwrap()
}

/// A worker run (`worker-runs/v1`): a codemode run attributed to agent-a
/// through `on_behalf_of_agent`, with no `agent_id`, under the worker's scope.
fn worker_run_stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind":"reserved", "module_id":PREFRONTAL},
        "ref":"worker-run-scope", "scope_epoch":1, "kind":"ephemeral",
        "attributes":{"on_behalf_of_agent":"agent-a", "run_id":"run-w"},
        "parent": {"owner": {"kind":"reserved", "module_id":PREFRONTAL}, "ref":"worker-scope", "scope_epoch":1},
        "parent_state":"linked",
        "owner_authorized":true,
        "expires_at_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 30000
    }))
    .unwrap()
}

/// Bind the worker's own classic route on `channel`, in `worker-session`.
async fn bind_worker(stream: &mut tokio::net::TcpStream, root: &Path, channel: u16) {
    bind_mode_as(
        stream,
        root,
        channel,
        Some(subc_protocol::call_mode::CallMode::Classic),
        Some(worker_stamp()),
        "worker-session",
    )
    .await;
}

/// Answer the worker run's pre-shell `scope.describe` with "live".
async fn answer_worker_run_live(stream: &mut tokio::net::TcpStream) {
    let request = read_frame_timeout(stream, "worker run describe").await;
    assert_eq!(
        request.header.ty,
        FrameType::Request,
        "{}",
        String::from_utf8_lossy(&request.body)
    );
    let body: subc_protocol::session::ModuleControlRequestFromModule =
        serde_json::from_slice(&request.body).unwrap();
    assert!(
        matches!(&body, subc_protocol::session::ModuleControlRequestFromModule::ScopeDescribe {scope_ref, ..}
        if scope_ref == "worker-run-scope"),
        "{body:?}"
    );
    let reply = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
        status: subc_protocol::scope::ScopeStatus::Live,
        scope_epoch: Some(1),
        daemon_incarnation: "daemon-a".into(),
        owner_synced: true,
        owner_configured: true,
        scope: Some(worker_run_stamp()),
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&reply).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

/// The error code a call's reply carries: a provider error frame's code, or
/// a tool response's native code.
fn any_code(frame: &Frame) -> String {
    if frame.header.ty == FrameType::Error {
        return reply_json(frame)["code"].as_str().unwrap_or("").to_owned();
    }
    reply_code(frame)
}

/// Send `name` with `preset` on `channel`.
async fn preset_call(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    preset: &str,
    name: &str,
    args: Value,
) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            1,
            corr,
            serde_json::to_vec(&json!({"name":name, "preset":preset, "arguments":args})).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

#[test]
fn worker_codemode_run_uses_the_workers_root_and_frozen_params_never_the_heads() {
    run_subc_bridge_test_with_dispatch(
        "worker-run-authority",
        Duration::from_secs(30),
        |input| async move {
            // Every route is held to its own root, so the head's root is
            // access only the head (and its agent runs) hold.
            std::fs::write(
                &input.user_config_path,
                serde_json::to_vec(&json!({"restrict_to_project_root": true})).unwrap(),
            )
            .unwrap();
            let mut daemon = open_scope_daemon_session(input).await;
            write_project_config(&daemon.root1, json!({}));
            write_project_config(&daemon.root2, json!({}));
            std::fs::write(daemon.root1.join("head.txt"), "head-root-file").unwrap();
            std::fs::write(daemon.root2.join("worker.txt"), "worker-root-file").unwrap();
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            bind_worker(&mut daemon.stream, &daemon.root2, 2).await;
            // An agent run of the same head, bound at the worker's root.
            bind_run(&mut daemon.stream, &daemon.root2, 3, stamp()).await;
            // The worker run binds at the head's root: its bind is
            // configuration, never authority.
            bind_run(&mut daemon.stream, &daemon.root1, 4, worker_run_stamp()).await;
            let head_file = json!({"path": daemon.root1.join("head.txt")});
            let worker_file = json!({"path": daemon.root2.join("worker.txt")});

            // The agent run behaves as before: the head's root is its root.
            named_call(&mut daemon.stream, 3, 100, "read", head_file.clone()).await;
            let reply = read_frame_timeout(&mut daemon.stream, "agent run head read").await;
            assert!(
                reply_text(&reply).contains("head-root-file"),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            // The worker run reaches the worker's root...
            named_call(&mut daemon.stream, 4, 101, "read", worker_file.clone()).await;
            let reply = read_frame_timeout(&mut daemon.stream, "worker run worker read").await;
            assert!(
                reply_text(&reply).contains("worker-root-file"),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            // ...and never the head's, although its own bind names it.
            named_call(&mut daemon.stream, 4, 102, "read", head_file.clone()).await;
            let reply = read_frame_timeout(&mut daemon.stream, "worker run head read").await;
            assert!(
                !reply_text(&reply).contains("head-root-file"),
                "{}",
                reply_text(&reply)
            );
            assert_eq!(
                any_code(&reply),
                "path_outside_root",
                "{}",
                reply_text(&reply)
            );

            // The worker freezes `confine: root` at its own catalog fetch.
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Request,
                    control_flags(),
                    2,
                    1,
                    103,
                    serde_json::to_vec(&json!({
                        "op":"tool.catalog", "preset":"worker", "params":{"confine":"root"}
                    }))
                    .unwrap(),
                )
                .unwrap(),
            )
            .await;
            let catalog = read_frame_timeout(&mut daemon.stream, "worker catalog").await;
            assert_eq!(
                catalog.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&catalog.body)
            );
            // A worker-run call naming the head preset is still held to the
            // worker's frozen params: a confined session offers no shell.
            preset_call(
                &mut daemon.stream,
                4,
                104,
                "head",
                "bash",
                json!({"command":"echo escaped", "timeout": 5000}),
            )
            .await;
            // A run's shell call first confirms the run is still live.
            answer_worker_run_live(&mut daemon.stream).await;
            let reply = read_frame_timeout(&mut daemon.stream, "confined worker run bash").await;
            assert!(
                !String::from_utf8_lossy(&reply.body).contains("escaped\n"),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            assert_eq!(
                any_code(&reply),
                "tool_not_offered_confined",
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            // Reads stay offered in a confined session, on the worker's root.
            preset_call(&mut daemon.stream, 4, 105, "head", "read", worker_file).await;
            let reply = read_frame_timeout(&mut daemon.stream, "confined worker run read").await;
            assert!(
                reply_text(&reply).contains("worker-root-file"),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        configured_dispatch,
    );
}

#[cfg(unix)]
#[test]
fn worker_codemode_run_cannot_see_or_kill_the_heads_bash_tasks() {
    run_subc_bridge_test_with_dispatch(
        "worker-run-head-tasks",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            bind_worker(&mut daemon.stream, &daemon.root2, 2).await;
            bind_run(&mut daemon.stream, &daemon.root1, 4, worker_run_stamp()).await;
            // The head starts a background task in its own session. The comment
            // names a path in this test's own root, so the observed-task lookup
            // finds this task and not another test's.
            let marker = daemon.root1.join("head-background-task");
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command": format!("sleep 30 # {}", marker.display()), "background":true}),
            )
            .await;
            let launch = read_frame_timeout(&mut daemon.stream, "head background launch").await;
            assert_eq!(
                launch.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&launch.body)
            );
            let (registry, task) = observed_task(&marker).await;
            assert!(registry
                .status(&task, "head-session", None, None, 0)
                .is_some());
            // The worker run looks the task up in the worker's session, where
            // it does not exist.
            named_call(
                &mut daemon.stream,
                4,
                101,
                "bash_status",
                json!({"taskId": &task}),
            )
            .await;
            let status = read_frame_timeout(&mut daemon.stream, "worker run bash_status").await;
            assert!(
                !reply_text(&status).contains("sleep 30"),
                "{}",
                String::from_utf8_lossy(&status.body)
            );
            assert!(
                reply_text(&status).contains("not found") || !any_code(&status).is_empty(),
                "{}",
                String::from_utf8_lossy(&status.body)
            );
            named_call(
                &mut daemon.stream,
                4,
                102,
                "bash_kill",
                json!({"taskId": &task}),
            )
            .await;
            let kill = read_frame_timeout(&mut daemon.stream, "worker run bash_kill").await;
            assert!(
                reply_text(&kill).contains("not found") || !any_code(&kill).is_empty(),
                "{}",
                String::from_utf8_lossy(&kill.body)
            );
            let snapshot = registry
                .status(&task, "head-session", None, None, 0)
                .unwrap();
            assert_eq!(
                snapshot.info.status,
                BgTaskStatus::Running,
                "a worker run must not kill the head's task"
            );
            // The head itself still controls its task.
            named_call(
                &mut daemon.stream,
                1,
                103,
                "bash_kill",
                json!({"taskId": &task}),
            )
            .await;
            let killed = read_frame_timeout(&mut daemon.stream, "head bash_kill").await;
            assert_eq!(
                killed.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&killed.body)
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        observing_dispatch,
    );
}

#[test]
fn worker_codemode_run_without_its_workers_identity_is_refused_by_name() {
    run_subc_bridge_test_with_dispatch(
        "worker-run-identity",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_scope_daemon_session(input).await;
            std::fs::write(daemon.root1.join("head.txt"), "head-root-file").unwrap();
            let read = json!({"path": daemon.root1.join("head.txt")});
            bind_head(&mut daemon.stream, &daemon.root1, 1, "head-session").await;
            // The worker's scope has no live route bound to AFT yet.
            bind_run(&mut daemon.stream, &daemon.root1, 2, worker_run_stamp()).await;
            let mut no_parent = worker_run_stamp();
            no_parent.scope_ref = "worker-run-no-parent".into();
            no_parent.parent = None;
            no_parent.parent_state = None;
            bind_run(&mut daemon.stream, &daemon.root1, 3, no_parent).await;
            let mut pending = worker_run_stamp();
            pending.scope_ref = "worker-run-pending".into();
            pending.parent_state = Some(subc_protocol::scope::ParentState::Pending);
            bind_run(&mut daemon.stream, &daemon.root1, 4, pending).await;
            // A worker run whose parent is the head's own scope.
            let mut under_head = worker_run_stamp();
            under_head.scope_ref = "worker-run-under-head".into();
            under_head.parent = stamp().parent;
            // (Channel 5's bind is the shared dispatch's configure-failure case.)
            bind_run(&mut daemon.stream, &daemon.root1, 7, under_head).await;
            for (channel, corr, reason) in [
                (2, 100, "worker_route_not_bound"),
                (3, 101, "parent_missing"),
                (4, 102, "parent_pending"),
                (7, 103, "parent_not_worker"),
            ] {
                named_call(&mut daemon.stream, channel, corr, "read", read.clone()).await;
                let error = expect_refusal(&mut daemon.stream, reason).await;
                assert_eq!(error.code, "run_owner_not_bound", "{reason}");
                let detail = error.detail.as_ref().unwrap();
                assert_eq!(detail["reason"], reason, "{detail}");
                assert_eq!(detail["on_behalf_of_agent"], "agent-a", "{detail}");
                assert_eq!(detail["run_id"], "run-w", "{detail}");
                assert!(!error.message.contains("head-root-file"));
            }
            // Once the worker's own route is bound, the same run is served,
            // on the worker's root.
            write_project_config(&daemon.root2, json!({}));
            std::fs::write(daemon.root2.join("worker.txt"), "worker-root-file").unwrap();
            bind_worker(&mut daemon.stream, &daemon.root2, 6).await;
            named_call(
                &mut daemon.stream,
                2,
                104,
                "read",
                json!({"path": daemon.root2.join("worker.txt")}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "served worker run").await;
            assert!(
                reply_text(&reply).contains("worker-root-file"),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}
