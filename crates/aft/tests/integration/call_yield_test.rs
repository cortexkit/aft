//! `tool.yield` (the tool-provider contract's `call-yield/v1`) over a real
//! route: a head agent's waiting `bash` call answers at once with its task id
//! while the command keeps running and later completes to its session as a
//! notice; a waiting `bash_watch` ends with what it has seen so far; every
//! other call answers `not_yieldable` or `unknown_call`, and nothing is ever
//! answered twice.

use super::*;
use std::path::Path;

const SESSION: &str = "yield-session";
const OTHER_SESSION: &str = "yield-other-session";

/// What a session runner declares when it opens a route that may yield.
const YIELD_CONSUMER: [&str; 3] = ["tool-result/v1", "held-reply/v1", "call-yield/v1"];

fn head_stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": "yield-head-scope", "scope_epoch": 1, "kind": "head",
        "attributes": {"agent_id": "agent-yield"},
        "owner_authorized": true
    }))
    .unwrap()
}

/// The real configure, and a `read` of a file named `yield-blocker*` that
/// holds until a sibling `.release` file exists, so a test can keep a
/// non-shell call open while it yields it.
fn yield_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    if req.command == "configure" {
        return aft::commands::configure::handle_configure(&req, ctx);
    }
    if req.command == "read" {
        let params = req.params.to_string();
        if let Some(start) = params.find("yield-blocker") {
            let name: String = params[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
                .collect();
            let root = ctx
                .config()
                .project_root
                .clone()
                .expect("a bound root has a project root");
            let release = root.join(format!("{name}.release"));
            let until = Instant::now() + Duration::from_secs(30);
            while !release.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    bridge_dispatch(req, ctx)
}

fn write_config(root: &Path) {
    std::fs::create_dir_all(root.join(".cortexkit")).unwrap();
    std::fs::write(
        root.join(".cortexkit/aft.jsonc"),
        serde_json::to_vec(&json!({
            "indexes": {"trigram": false, "semantic": false, "callgraph": false},
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Bind route `channel` as a head agent in `session` on tool-provider v1.
/// `consumer` is the route's consumer capabilities.
async fn bind_head(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    session: &str,
    principal: Principal,
    consumer: &[&str],
) {
    bind_scoped(
        stream,
        root,
        channel,
        session,
        principal,
        consumer,
        head_stamp(),
    )
    .await;
}

async fn bind_scoped(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    session: &str,
    principal: Principal,
    consumer: &[&str],
    scope: subc_protocol::scope::ScopeStamp,
) {
    let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new("opencode", session),
        principal: Some(principal),
        consumer_capabilities: Some(consumer.iter().map(|cap| cap.to_string()).collect()),
        admission_facts: Default::default(),
        scope: Some(scope),
        role_versions: Some(std::collections::BTreeMap::from([(
            "tool-provider".into(),
            "v1".into(),
        )])),
        call_mode: Some(subc_protocol::call_mode::CallMode::Classic),
    })
    .unwrap();
    bind["identity"]["project_root"] = json!(root);
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            channel as u64,
            serde_json::to_vec(&bind).unwrap(),
        )
        .unwrap(),
    )
    .await;
    expect_route_bind_ack(stream, channel as u64).await;
}

async fn request(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64, body: Value) {
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

/// A keyed head call of `tool`.
async fn call(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    tool: &str,
    key: &str,
    arguments: Value,
) {
    request(
        stream,
        channel,
        corr,
        json!({"name": tool, "preset": "head", "call_key": key, "arguments": arguments}),
    )
    .await;
}

async fn send_yield(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64, key: &str) {
    send_yield_reason(stream, channel, corr, key, "person_message").await;
}

async fn send_yield_reason(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    key: &str,
    reason: &str,
) {
    request(
        stream,
        channel,
        corr,
        json!({"name": "tool.yield", "arguments": {"call_key": key, "reason": reason}}),
    )
    .await;
}

/// Frames read off the connection, kept so a test can wait for one frame
/// without losing the others that arrive meanwhile.
#[derive(Default)]
struct Inbox {
    frames: Vec<Frame>,
}

impl Inbox {
    /// The first frame matching `want`, reading more until `within` passes.
    async fn take(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        within: Duration,
        label: &str,
        want: impl Fn(&Frame) -> bool,
    ) -> Frame {
        let deadline = Instant::now() + within;
        loop {
            if let Some(at) = self.frames.iter().position(&want) {
                return self.frames.remove(at);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for {label}");
            match tokio::time::timeout(remaining, read_subc_frame(stream)).await {
                Ok(frame) => self.frames.push(
                    frame
                        .expect("read frame")
                        .unwrap_or_else(|| panic!("EOF waiting for {label}")),
                ),
                Err(_) => panic!("timed out waiting for {label}"),
            }
        }
    }

    /// The terminal frame answering `corr` on `channel`.
    async fn reply(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        channel: u16,
        corr: u64,
        within: Duration,
        label: &str,
    ) -> Frame {
        self.take(stream, within, label, |frame| {
            frame.header.ty != FrameType::Push
                && frame.header.channel == channel
                && frame.header.corr == corr
        })
        .await
    }

    /// Read whatever arrives during `period`.
    async fn collect_for(&mut self, stream: &mut tokio::net::TcpStream, period: Duration) {
        let deadline = Instant::now() + period;
        while let Ok(frame) = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            read_subc_frame(stream),
        )
        .await
        {
            self.frames
                .push(frame.expect("read frame").expect("connection stays open"));
        }
    }
}

fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap_or(Value::Null)
}

/// The yield answer in `frame`.
fn answer(frame: &Frame) -> Value {
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "{}",
        String::from_utf8_lossy(&frame.body)
    );
    body(frame)
}

/// Every text part of a tool result, joined.
fn result_text(frame: &Frame) -> String {
    let value = body(frame);
    match value["content"].as_array() {
        Some(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::from_utf8_lossy(&frame.body).into_owned(),
    }
}

/// The task id a handed-back bash reply names.
fn handed_back_task(text: &str) -> String {
    let start = text
        .find("running in background as ")
        .unwrap_or_else(|| panic!("no task in {text}"))
        + "running in background as ".len();
    text[start..]
        .split(|c: char| c == '.' || c.is_whitespace())
        .next()
        .unwrap()
        .to_owned()
}

fn alive(pid: i32) -> bool {
    // Signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait until the route's `aft.db` is open: keyed calls are refused with
/// `database_initializing` until then, and the bind acknowledgement comes
/// before the database opens.
async fn wait_for_ledger(
    stream: &mut tokio::net::TcpStream,
    inbox: &mut Inbox,
    channel: u16,
    first_corr: u64,
) {
    let until = Instant::now() + Duration::from_secs(15);
    let mut corr = first_corr;
    loop {
        request(
            stream,
            channel,
            corr,
            json!({"name": "late_results", "arguments": {}}),
        )
        .await;
        let ready = inbox
            .reply(
                stream,
                channel,
                corr,
                Duration::from_secs(10),
                "ledger readiness",
            )
            .await;
        if ready.header.ty == FrameType::Response {
            return;
        }
        assert_eq!(
            body(&ready)["code"],
            "database_initializing",
            "{}",
            String::from_utf8_lossy(&ready.body)
        );
        assert!(Instant::now() < until, "aft.db never opened");
        corr += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn read_pid(path: &Path) -> i32 {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < until, "the command never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A command that records its shell's pid, prints a line, then runs until
/// `release` exists.
fn held_command(pid: &Path, release: &Path) -> String {
    format!(
        "echo $$ > '{}'; echo started; {}; echo released",
        pid.display(),
        crate::test_helpers::test_gate::gate(&format!("'{}'", release.display()), "sleep 0.05")
    )
}

fn unique(name: &str) -> String {
    format!(
        "{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Yield `key` until it is accepted. A call answers `not_waiting` until it
/// reaches its wait (the command can print before the spawn job reports
/// back), which a runner would simply wait out.
async fn yield_until_accepted(
    stream: &mut tokio::net::TcpStream,
    inbox: &mut Inbox,
    channel: u16,
    first_corr: u64,
    key: &str,
) -> (u64, Instant) {
    yield_reason_until_accepted(stream, inbox, channel, first_corr, key, "person_message").await
}

async fn yield_reason_until_accepted(
    stream: &mut tokio::net::TcpStream,
    inbox: &mut Inbox,
    channel: u16,
    first_corr: u64,
    key: &str,
    reason: &str,
) -> (u64, Instant) {
    let until = Instant::now() + Duration::from_secs(15);
    let mut corr = first_corr;
    loop {
        let sent = Instant::now();
        send_yield_reason(stream, channel, corr, key, reason).await;
        let reply = answer(
            &inbox
                .reply(
                    stream,
                    channel,
                    corr,
                    Duration::from_secs(5),
                    "yield answer",
                )
                .await,
        );
        if reply == json!({"answer": "accepted"}) {
            return (corr, sent);
        }
        if reply != json!({"answer": "not_yieldable", "reason": "not_waiting"}) {
            inbox.collect_for(stream, Duration::from_millis(100)).await;
        }
        assert_eq!(
            reply,
            json!({"answer": "not_yieldable", "reason": "not_waiting"}),
            "only a call still reaching its wait may refuse: key={key}, queued={:?}",
            inbox.frames.iter().map(body).collect::<Vec<_>>()
        );
        assert!(Instant::now() < until, "the call never became yieldable");
        corr += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A head's foreground `bash` waiting for its command hands back on
/// `tool.yield`: the call answers within the contract's five seconds with
/// the task id, the command keeps running, a second yield is
/// `unknown_call`, and the command's completion reaches the launching
/// session's route as a notice, and no other route.
#[test]
fn yielded_foreground_bash_answers_at_once_and_completes_to_its_session() {
    run_subc_bridge_test_with_dispatch(
        "call-yield-bash",
        Duration::from_secs(90),
        |input| async move {
            let (mut daemon, _) = open_fake_daemon_session_with_ops(input, &[]).await;
            let root = daemon.root1.clone();
            write_config(&root);
            let stream = &mut daemon.stream;
            bind_head(
                stream,
                &root,
                1,
                SESSION,
                Principal::Direct,
                &YIELD_CONSUMER,
            )
            .await;
            // The same session under another principal, and another session:
            // neither launched the command, so neither may get its notice.
            bind_head(
                stream,
                &root,
                2,
                SESSION,
                Principal::Reserved {
                    module_id: "basal".into(),
                },
                &YIELD_CONSUMER,
            )
            .await;
            bind_head(
                stream,
                &root,
                3,
                OTHER_SESSION,
                Principal::Direct,
                &YIELD_CONSUMER,
            )
            .await;
            let mut inbox = Inbox::default();
            wait_for_ledger(stream, &mut inbox, 1, 900).await;
            let key = unique("yield-bash");
            let pid_path = root.join("yield-bash.pid");
            let release = root.join("yield-bash.release");
            call(
                stream,
                1,
                100,
                "bash",
                &key,
                json!({"command": held_command(&pid_path, &release), "timeout": 60_000}),
            )
            .await;
            let pid = read_pid(&pid_path).await;

            let (_, yielded_at) = yield_until_accepted(stream, &mut inbox, 1, 200, &key).await;
            let reply = inbox
                .reply(
                    stream,
                    1,
                    100,
                    Duration::from_secs(5),
                    "handed-back bash reply",
                )
                .await;
            assert!(
                yielded_at.elapsed() < Duration::from_secs(5),
                "the final frame came {:?} after the yield",
                yielded_at.elapsed()
            );
            assert_eq!(reply.header.ty, FrameType::Response);
            let text = result_text(&reply);
            assert!(
                text.contains("Detached because a user message arrived."),
                "{text}"
            );
            let task = handed_back_task(&text);
            assert!(task.starts_with("bash-"), "{text}");
            assert!(alive(pid), "a yield never stops the command");

            // The call has its final frame: a repeat finds no open call.
            send_yield(stream, 1, 300, &key).await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 300, Duration::from_secs(5), "second yield")
                        .await
                ),
                json!({"answer": "unknown_call"})
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(alive(pid), "the command still runs after the call ended");

            std::fs::write(&release, "").unwrap();
            let notice = inbox
                .take(
                    stream,
                    Duration::from_secs(30),
                    "completion notice",
                    |frame| {
                        frame.header.ty == FrameType::Push
                            && body(frame)["type"] == "bash_completed"
                            && body(frame)["task_id"] == task.as_str()
                    },
                )
                .await;
            assert_eq!(notice.header.channel, 1, "{}", body(&notice));
            inbox.collect_for(stream, Duration::from_millis(500)).await;
            assert!(
                !inbox.frames.iter().any(|frame| {
                    frame.header.ty == FrameType::Push && body(frame)["task_id"] == task.as_str()
                }),
                "the completion reached a route that did not launch the command"
            );
            assert!(
                !inbox
                    .frames
                    .iter()
                    .any(|frame| frame.header.ty != FrameType::Push && frame.header.corr == 100),
                "the call was answered twice"
            );
            send_connection_goodbye(stream).await;
        },
        |_, _, _| {},
        yield_dispatch,
    );
}

/// A head's `bash_watch` waiting on a running task hands back on
/// `tool.yield` the way a watch whose limit passed ends: it reports how long
/// it waited and that the command still runs. The task is left running.
#[test]
fn yielded_bash_watch_ends_with_what_it_has_seen_and_leaves_the_task_running() {
    run_subc_bridge_test_with_dispatch(
        "call-yield-watch",
        Duration::from_secs(90),
        |input| async move {
            let (mut daemon, _) = open_fake_daemon_session_with_ops(input, &[]).await;
            let root = daemon.root1.clone();
            write_config(&root);
            let stream = &mut daemon.stream;
            bind_head(
                stream,
                &root,
                1,
                SESSION,
                Principal::Direct,
                &YIELD_CONSUMER,
            )
            .await;
            let mut inbox = Inbox::default();
            wait_for_ledger(stream, &mut inbox, 1, 900).await;
            let pid_path = root.join("yield-watch.pid");
            let release = root.join("yield-watch.release");
            // A foreground call handed back is the way a head on a v1 route
            // gets a running background task to watch.
            let bash_key = unique("yield-watch-bash");
            call(
                stream,
                1,
                100,
                "bash",
                &bash_key,
                json!({"command": held_command(&pid_path, &release), "timeout": 60_000}),
            )
            .await;
            let pid = read_pid(&pid_path).await;
            yield_until_accepted(stream, &mut inbox, 1, 200, &bash_key).await;
            let task = handed_back_task(&result_text(
                &inbox
                    .reply(stream, 1, 100, Duration::from_secs(5), "handed-back bash")
                    .await,
            ));

            let watch_key = unique("yield-watch");
            let watch_sent = Instant::now();
            call(
                stream,
                1,
                400,
                "bash_watch",
                &watch_key,
                json!({"taskId": task, "timeoutMs": 60_000}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (_, yielded_at) =
                yield_until_accepted(stream, &mut inbox, 1, 500, &watch_key).await;
            let reply = inbox
                .reply(stream, 1, 400, Duration::from_secs(5), "handed-back watch")
                .await;
            assert!(yielded_at.elapsed() < Duration::from_secs(5));
            assert!(
                watch_sent.elapsed() < Duration::from_secs(30),
                "the watch ended long before its own limit"
            );
            assert_eq!(reply.header.ty, FrameType::Response);
            let text = result_text(&reply);
            assert!(text.contains(&format!("Task {task}: running")), "{text}");
            assert!(text.contains("timeout reached without match"), "{text}");
            assert!(text.contains("The command is still running"), "{text}");
            assert!(alive(pid), "a yielded watch leaves its task running");

            send_yield(stream, 1, 600, &watch_key).await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 600, Duration::from_secs(5), "second yield")
                        .await
                ),
                json!({"answer": "unknown_call"})
            );
            std::fs::write(&release, "").unwrap();
            send_connection_goodbye(stream).await;
        },
        |_, _, _| {},
        yield_dispatch,
    );
}

/// Calls that cannot hand back say so and are left alone: a call of another
/// tool and a call on a route whose consumer did not opt in answer
/// `not_yieldable`, and a call that already finished, or a key never used,
/// answers `unknown_call`.
#[test]
fn other_calls_are_not_yieldable_and_finished_calls_are_unknown() {
    run_subc_bridge_test_with_dispatch(
        "call-yield-refusals",
        Duration::from_secs(90),
        |input| async move {
            let (mut daemon, _) = open_fake_daemon_session_with_ops(input, &[]).await;
            let root = daemon.root1.clone();
            write_config(&root);
            let stream = &mut daemon.stream;
            bind_head(
                stream,
                &root,
                1,
                SESSION,
                Principal::Direct,
                &YIELD_CONSUMER,
            )
            .await;
            // The same head, opened without `call-yield/v1`.
            bind_head(
                stream,
                &root,
                2,
                SESSION,
                Principal::Direct,
                &["tool-result/v1", "held-reply/v1"],
            )
            .await;
            let mut inbox = Inbox::default();
            wait_for_ledger(stream, &mut inbox, 1, 900).await;

            // A read held open by the dispatch.
            let blocker = root.join("yield-blocker.txt");
            std::fs::write(&blocker, "blocked\n").unwrap();
            let read_key = unique("yield-read");
            request(stream, 1, 104, json!({"name":"read", "preset":"head", "arguments":{"path":blocker.display().to_string()}})).await;
            call(
                stream,
                1,
                100,
                "read",
                &read_key,
                json!({"path": blocker.display().to_string()}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            send_yield(stream, 1, 101, &read_key).await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 101, Duration::from_secs(5), "read yield")
                        .await
                ),
                json!({"answer": "not_yieldable", "reason": "tool_not_yieldable"})
            );
            for (corr, reason, expected) in [
                (102, "runner_restart", "tool_not_yieldable"),
                (103, "unrecognized", "reason_not_supported"),
            ] {
                send_yield_reason(stream, 1, corr, &read_key, reason).await;
                assert_eq!(
                    answer(
                        &inbox
                            .reply(stream, 1, corr, Duration::from_secs(5), "restart refusal")
                            .await
                    ),
                    json!({"answer":"not_yieldable", "reason":expected})
                );
            }
            send_yield_reason(stream, 1, 105, "keyless", "runner_restart").await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 105, Duration::from_secs(5), "keyless restart")
                        .await
                ),
                json!({"answer":"unknown_call"})
            );
            std::fs::write(root.join("yield-blocker.txt.release"), "").unwrap();
            inbox
                .reply(
                    stream,
                    1,
                    104,
                    Duration::from_secs(30),
                    "keyless read result",
                )
                .await;
            let read = inbox
                .reply(stream, 1, 100, Duration::from_secs(30), "read result")
                .await;
            assert!(
                result_text(&read).contains("blocked"),
                "{}",
                result_text(&read)
            );

            // A waiting bash on the route that did not opt in.
            let pid_path = root.join("yield-plain.pid");
            let release = root.join("yield-plain.release");
            let plain_key = unique("yield-plain");
            call(
                stream,
                2,
                200,
                "bash",
                &plain_key,
                json!({"command": held_command(&pid_path, &release), "timeout": 60_000}),
            )
            .await;
            let pid = read_pid(&pid_path).await;
            send_yield(stream, 2, 201, &plain_key).await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 2, 201, Duration::from_secs(5), "plain yield")
                        .await
                ),
                json!({"answer": "not_yieldable", "reason": "not_negotiated"})
            );
            assert!(alive(pid));
            send_yield_reason(stream, 2, 202, &plain_key, "runner_restart").await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(
                            stream,
                            2,
                            202,
                            Duration::from_secs(5),
                            "unnegotiated restart"
                        )
                        .await
                ),
                json!({"answer":"not_yieldable", "reason":"not_negotiated"})
            );
            std::fs::write(&release, "").unwrap();
            let finished = inbox
                .reply(stream, 2, 200, Duration::from_secs(30), "plain bash result")
                .await;
            let text = result_text(&finished);
            assert!(text.contains("released"), "{text}");

            // A call that already finished, on the opted-in route.
            let done_key = unique("yield-done");
            call(
                stream,
                1,
                300,
                "bash",
                &done_key,
                json!({"command": "echo done"}),
            )
            .await;
            let done = inbox
                .reply(stream, 1, 300, Duration::from_secs(30), "finished bash")
                .await;
            assert!(
                result_text(&done).contains("done"),
                "{}",
                result_text(&done)
            );
            send_yield(stream, 1, 301, &done_key).await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 301, Duration::from_secs(5), "finished yield")
                        .await
                ),
                json!({"answer": "unknown_call"})
            );
            // Another route's call is not this route's to yield.
            send_yield(stream, 1, 302, &plain_key).await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 302, Duration::from_secs(5), "foreign yield")
                        .await
                ),
                json!({"answer": "unknown_call"})
            );
            inbox.collect_for(stream, Duration::from_millis(300)).await;
            assert!(
                !inbox.frames.iter().any(|frame| {
                    frame.header.ty != FrameType::Push
                        && [100, 200, 300].contains(&frame.header.corr)
                }),
                "a call was answered twice"
            );
            send_connection_goodbye(stream).await;
        },
        |_, _, _| {},
        yield_dispatch,
    );
}

fn restart_worker_stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind":"reserved", "module_id":"prefrontal-core"},
        "ref":"restart-worker", "scope_epoch":1, "kind":"worker",
        "parent": {"owner":{"kind":"reserved", "module_id":"prefrontal-core"}, "ref":"yield-head-scope", "scope_epoch":1},
        "parent_state":"linked", "owner_authorized":true
    })).unwrap()
}

async fn late_result(
    stream: &mut tokio::net::TcpStream,
    inbox: &mut Inbox,
    channel: u16,
    key: &str,
) -> Value {
    let until = Instant::now() + Duration::from_secs(30);
    let mut corr = 10_000;
    loop {
        request(
            stream,
            channel,
            corr,
            json!({"name":"late_results", "arguments":{}}),
        )
        .await;
        let page = answer(
            &inbox
                .reply(
                    stream,
                    channel,
                    corr,
                    Duration::from_secs(5),
                    "late results",
                )
                .await,
        );
        if let Some(entry) = page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["call_key"] == key)
        {
            assert_eq!(entry["kind"], "result", "{entry}");
            assert_ne!(entry["reduced"], true, "{entry}");
            return entry["result"].clone();
        }
        assert!(
            Instant::now() < until,
            "no terminal late result for {key}: {page}"
        );
        corr += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn runner_restart_holds_worker_bash_and_watch_until_their_real_results() {
    run_subc_bridge_test_with_dispatch(
        "call-yield-runner-restart",
        Duration::from_secs(90),
        |input| async move {
            let (mut daemon, _) = open_fake_daemon_session_with_ops(input, &[]).await;
            let root = daemon.root1.clone();
            write_config(&root);
            let stream = &mut daemon.stream;
            bind_scoped(
                stream,
                &root,
                1,
                SESSION,
                Principal::Direct,
                &YIELD_CONSUMER,
                restart_worker_stamp(),
            )
            .await;
            let mut inbox = Inbox::default();
            wait_for_ledger(stream, &mut inbox, 1, 900).await;
            let pid_path = root.join("restart.pid");
            let release = root.join("restart.release");
            let bash_key = unique("restart-bash");
            request(stream, 1, 100, json!({"name":"bash", "preset":"worker", "call_key":bash_key,
                "arguments":{"command":held_command(&pid_path, &release), "wait":true, "timeout":60000}})).await;
            let pid = read_pid(&pid_path).await;
            let (_, sent) = yield_reason_until_accepted(
                stream,
                &mut inbox,
                1,
                200,
                &bash_key,
                "runner_restart",
            )
            .await;
            let held = answer(
                &inbox
                    .reply(stream, 1, 100, Duration::from_secs(5), "worker held bash")
                    .await,
            );
            assert!(sent.elapsed() < Duration::from_secs(5));
            assert_eq!(held["held"]["reason"], "running");
            assert_eq!(held["held"]["yielded"], true);
            assert_eq!(held["content"], json!([{"type":"text", "text":""}]));
            assert!(alive(pid));
            let task = held["held"]["detail"]["task_id"]
                .as_str()
                .expect("held task id");
            let watch_key = unique("restart-watch");
            request(
                stream,
                1,
                400,
                json!({"name":"bash_watch", "preset":"worker", "call_key":watch_key,
                "arguments":{"taskId":task, "timeoutMs":60000}}),
            )
            .await;
            let (_, sent) = yield_reason_until_accepted(
                stream,
                &mut inbox,
                1,
                500,
                &watch_key,
                "runner_restart",
            )
            .await;
            let watch_held = answer(
                &inbox
                    .reply(stream, 1, 400, Duration::from_secs(5), "worker held watch")
                    .await,
            );
            assert!(sent.elapsed() < Duration::from_secs(5));
            assert_eq!(
                watch_held["held"],
                json!({"reason":"running", "yielded":true})
            );
            assert!(alive(pid));
            send_yield_reason(stream, 1, 600, &watch_key, "runner_restart").await;
            assert_eq!(
                answer(
                    &inbox
                        .reply(stream, 1, 600, Duration::from_secs(5), "repeat yield")
                        .await
                ),
                json!({"answer":"unknown_call"})
            );
            std::fs::write(&release, "").unwrap();
            let bash_result = late_result(stream, &mut inbox, 1, &bash_key).await;
            let watch_result = late_result(stream, &mut inbox, 1, &watch_key).await;
            for result in [&bash_result, &watch_result] {
                assert!(result.get("held").is_none(), "{result}");
                assert_ne!(result["isError"], true, "{result}");
                assert!(
                    result["content"].to_string().contains("released"),
                    "{result}"
                );
                assert!(
                    !result["content"].to_string().contains("Detached because"),
                    "{result}"
                );
            }
            assert!(
                watch_result["content"].to_string().contains("task exited"),
                "{watch_result}"
            );
            inbox.collect_for(stream, Duration::from_millis(500)).await;
            assert!(
                !inbox
                    .frames
                    .iter()
                    .any(|frame| frame.header.ty == FrameType::Push
                        && body(frame)["type"] == "bash_completed"),
                "restart yields must not publish completion notices"
            );
            assert!(
                !inbox
                    .frames
                    .iter()
                    .any(|frame| frame.header.ty != FrameType::Push
                        && matches!(frame.header.corr, 100 | 400)),
                "a held call received a second terminal frame"
            );
            send_connection_goodbye(stream).await;
        },
        |_, _, _| {},
        yield_dispatch,
    );
}
