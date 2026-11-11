//! A `background` bash launch by a top-level (head) agent on one of Thalamus's
//! runners answers at once with its task id, and the task's completion reaches that
//! head's route later as a notice. Other server-owned background calls and
//! every foreground call keep holding until their commands end. PTY launches
//! always answer at once so any caller can drive its interactive command.

use super::super::test_support::*;
use super::*;
use crate::bash_background::BgTaskStatus;

const SESSION: &str = "runner-head-session";

/// A first-party tool-provider v1 route in [`SESSION`], bound by `principal`
/// for `harness` under a scope of `kind` (`head` or `worker`).
fn v1_route(root: &ProjectRootId, principal: &str, harness: &str, kind: &str) -> RouteIdentity {
    let mut route = route_identity_with_harness(root, SESSION, BindTrust::FirstParty, harness);
    let data = Arc::get_mut(&mut route.0).unwrap();
    data.role = tool_provider::RouteRole::ToolProviderV1;
    if let AuthenticatedPrincipal::RouteBind {
        principal_id,
        harness: bound_harness,
        ..
    } = &mut data.spawn_principal
    {
        *principal_id = Some(principal.to_string());
        *bound_harness = harness.to_string();
    }
    data.scope = Some(
        serde_json::from_value(json!({
            "owner": {"kind":"reserved", "module_id":"prefrontal-core"},
            "ref": format!("{kind}-scope"), "scope_epoch": 1, "kind": kind,
            "owner_authorized": true,
        }))
        .unwrap(),
    );
    route
}

/// The OpenCode plugin's own route for the same session id: a different
/// principal and harness, so a different launching identity.
fn plugin_route(root: &ProjectRootId) -> RouteIdentity {
    let mut route = route_identity_with_harness(root, SESSION, BindTrust::FirstParty, "opencode");
    let data = Arc::get_mut(&mut route.0).unwrap();
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
        *principal_id = Some("direct".into());
    }
    route
}

struct Fixture {
    dir: tempfile::TempDir,
    _storage: tempfile::TempDir,
    root: ProjectRootId,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    notices: mpsc::UnboundedReceiver<PushFrame>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let (dir, root) = test_root(name);
        let storage = tempfile::tempdir().unwrap();
        let (notice_tx, notices) = mpsc::unbounded_channel();
        let sender: ProgressSender = Arc::new(Box::new(move |frame| {
            let _ = notice_tx.send(frame);
        }) as Box<dyn Fn(PushFrame) + Send + Sync>);
        let ctx = test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.storage_dir = Some(storage.path().into());
            config.experimental_bash_background = true;
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
            config.sandbox.profile.worker = crate::config::SandboxMode::Off;
        });
        ctx.set_progress_sender(Some(sender));
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        Self {
            dir,
            _storage: storage,
            root,
            ctx,
            executor,
            notices,
        }
    }

    /// The path whose creation lets [`Self::blocked_command`] finish.
    fn release_path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    /// A command that runs until [`Self::release`] is called with `name`.
    fn blocked_command(&self, name: &str) -> String {
        format!(
            "while [ ! -f '{}' ]; do sleep 0.05; done; echo released",
            self.release_path(name).display()
        )
    }

    fn release(&self, name: &str) {
        std::fs::write(self.release_path(name), "").unwrap();
    }

    /// Submit a bash call on `route` exactly as the frame loop does for a
    /// tool-provider route: the route identity installed for the call, the
    /// worker flags from the call's preset, and server completion set because
    /// the route is v1.
    fn call(
        &self,
        route: &RouteIdentity,
        worker: bool,
        arguments: Value,
    ) -> (
        mpsc::Receiver<bash::BashDeferredCompletion>,
        bash::BashWaitCancel,
    ) {
        let (done_tx, done_rx) = mpsc::channel(4);
        let (touch_tx, _touch_rx) = mpsc::channel(4);
        let cancel = bash::BashWaitCancel {
            keepalive: None,
            speech_seed: None,
            run_scope: None,
            scope: route.scope.clone(),
            late_results: true,
            ledger_carrier: None,
            connection: PersistentCancelSignal::new(),
            route: PersistentCancelSignal::new(),
            drain: drain::ModuleDrainWindow::default(),
        };
        let _identity = secret_consent::SecretIdentityGuard::install(route.clone());
        bash::submit_deferred_bash(
            false,
            &self.executor,
            &done_tx,
            &touch_tx,
            &Arc::new(DispatchPathMetrics::new()),
            |request, ctx| crate::commands::bash::handle(&request, ctx),
            self.root.clone(),
            self.root.as_path().into(),
            route.session.clone(),
            "runner-head-call".into(),
            route_key(1, 1),
            1,
            control_flags(),
            PROTOCOL_VERSION,
            arguments,
            crate::subc_format::FormatContext::default(),
            cancel.clone(),
            route.trust,
            route.spawn_principal.clone(),
            None,
            Some("runner-head-call".into()),
            None,
            None,
            None,
            worker,
            worker,
            false,
            route.role == tool_provider::RouteRole::ToolProviderV1,
            remote_policy::RemoteSource::None,
            crate::commands::blocking_wait::ReplyBudget::new(
                Instant::now(),
                Some(Instant::now() + Duration::from_secs(120)),
            ),
        );
        (done_rx, cancel)
    }

    fn status(&self, task_id: &str) -> BgTaskStatus {
        self.ctx
            .bash_background()
            .observed_status(task_id, SESSION, 0)
            .expect("the session reads its task")
            .info
            .status
    }

    async fn wait_terminal(&self, task_id: &str) {
        let registry = self.ctx.bash_background().clone();
        tokio::time::timeout(Duration::from_secs(30), async {
            while !registry.is_task_terminal(task_id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the task finished");
    }

    /// The completion notice the registry pushed for `task_id`.
    async fn completion(&mut self, task_id: &str) -> PushFrame {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(30), self.notices.recv())
                .await
                .expect("a completion notice is pushed")
                .expect("the notice channel stays open");
            if matches!(&frame, PushFrame::BashCompleted(frame) if frame.task_id == task_id) {
                return frame;
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.ctx.bash_background().shutdown();
    }
}

/// The call's single reply, which must arrive within `within`.
async fn reply_within(
    replies: &mut mpsc::Receiver<bash::BashDeferredCompletion>,
    within: Duration,
) -> bash::BashDeferredCompletion {
    tokio::time::timeout(within, replies.recv())
        .await
        .expect("the call answered in time")
        .expect("the reply channel stays open")
}

/// Assert `reply` is the ordinary background launch reply, and return its
/// task id.
fn launch_task_id(reply: &bash::BashDeferredCompletion) -> String {
    let response = reply.response_for_test();
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["status"], "running", "{response:?}");
    let text = reply.text_for_test().unwrap();
    assert!(text.starts_with("Background task started"), "{text}");
    response.data["task_id"].as_str().unwrap().to_owned()
}

/// Deliver `completion` through the reliable push fan-out to `routes`, all
/// indexed under `root`, and return the channels that received it.
fn delivered_channels(
    root: &ProjectRootId,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    completion: &PushFrame,
) -> Vec<RouteChannel> {
    let root_channels = HashMap::from([(root.clone(), routes.keys().copied().collect())]);
    let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);
    fan_out_reliable_push_frame(
        &writer_tx,
        &DispatchPathMetrics::new(),
        routes,
        &root_channels,
        &HashMap::new(),
        &mut RetryBuffer::new(),
        &mut HashMap::new(),
        root,
        completion,
        None,
    );
    let mut channels = Vec::new();
    while let Ok(frame) = writer_rx.try_recv() {
        assert_eq!(
            push_frame_task_id(&frame),
            completion_task(completion).map(str::to_owned)
        );
        channels.push(route_key(frame.header.channel, frame.header.epoch));
    }
    channels
}

/// A background launch on a Thalamus runner's head route answers at once;
/// its task survives the call's route closing and its connection dropping,
/// finishes on its own, and its completion is sent once as a runner notice,
/// without a completion frame on the tool route.
async fn assert_runner_head_background_hands_back(principal: &str, harness: &str) {
    let mut fixture = Fixture::new("runner-head-background");
    let route = v1_route(&fixture.root, principal, harness, "head");
    let (mut replies, cancel) = fixture.call(
        &route,
        false,
        json!({"command": fixture.blocked_command("release"), "background": true, "timeout": 60_000}),
    );
    // The command cannot finish before the release file exists, so a reply
    // now proves the call did not hold until the command ended.
    let reply = reply_within(&mut replies, Duration::from_secs(15)).await;
    let task_id = launch_task_id(&reply);
    assert_eq!(fixture.status(&task_id), BgTaskStatus::Running);

    // The reply has gone out; the route then closes and the connection drops.
    cancel.route.cancel();
    cancel.connection.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fixture.status(&task_id),
        BgTaskStatus::Running,
        "a handed-back task outlives its call's route"
    );
    assert!(replies.try_recv().is_err(), "the call answers exactly once");

    fixture.release("release");
    fixture.wait_terminal(&task_id).await;
    assert_eq!(fixture.status(&task_id), BgTaskStatus::Completed);
    let completion = fixture.completion(&task_id).await;
    let PushFrame::BashCompleted(frame) = &completion else {
        unreachable!()
    };
    assert_eq!(frame.launching_identity, Some(route.task_identity()));
    assert_eq!(frame.exit_code, Some(0));
    let owner = route_key(1, 1);
    let routes = HashMap::from([(owner, route.clone())]);
    assert!(delivered_channels(&fixture.root, &routes, &completion).is_empty());
    let capture = super::super::runner_notices::test_endpoint::capture(&task_id, true);
    let metrics = DispatchPathMetrics::new();
    metrics
        .completion_delivery
        .registries
        .lock()
        .unwrap()
        .insert(fixture.root.clone(), fixture.ctx.bash_background().clone());
    let (writer, mut output) = mpsc::channel(8);
    let channels = HashMap::from([(fixture.root.clone(), HashSet::from([owner]))]);
    for _ in 0..2 {
        process_reliable_push_frame(
            &writer,
            &metrics,
            &routes,
            &channels,
            &HashMap::new(),
            &mut RetryBuffer::new(),
            &mut HashMap::new(),
            &mut CompletedTaskIds::default(),
            fixture.root.clone(),
            completion.clone(),
            None,
        );
    }
    let first = capture
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let retry = capture
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert_eq!(first.harness, harness);
    assert_eq!(first.session, route.session);
    assert_eq!(
        first.scope,
        route
            .scope
            .as_ref()
            .map(|scope| subc_protocol::scope::ScopeSelector::new(
                scope.owner.clone(),
                scope.scope_ref.clone(),
                Some(scope.scope_epoch)
            ))
    );
    assert_eq!(first.request, retry.request);
    assert_eq!(first.request["params"]["prompt"], super::super::bash_notice_text::completion(&serde_json::to_value(&completion).unwrap()), "notice rendering must not enrich the former frame with persisted duration or other metadata");
    assert_eq!(
        first.request["params"]["send_id"],
        format!("aft:bash:{task_id}")
    );
    assert_eq!(
        first.request["params"]["delivery"],
        json!(["tool_result", "steer", "idle_start"])
    );
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(output.try_recv().is_err());
}

#[tokio::test]
async fn thalamus_harness_head_background_answers_at_once_and_completes_to_its_route() {
    assert_runner_head_background_hands_back("reserved:thalamus-harness", "opencode2").await;
}

#[tokio::test]
async fn thalamus_harness_pi_head_background_answers_at_once_and_sends_a_notice() {
    assert_runner_head_background_hands_back("reserved:thalamus-harness", "pi").await;
}

async fn assert_pty_launch_hands_back(principal: &str, harness: &str, kind: &str, worker: bool) {
    let fixture = Fixture::new("pty-launch-handback");
    let route = v1_route(&fixture.root, principal, harness, kind);
    let received = fixture.release_path("received-input");
    let (mut replies, cancel) = fixture.call(
        &route,
        worker,
        json!({
            "command": format!(
                "IFS= read -r line; printf '%s' \"$line\" > '{}'",
                received.display()
            ),
            "pty": true,
            "timeout": 60_000,
        }),
    );
    // The command needs stdin to finish, so only a launch reply can arrive
    // before bash_write supplies the input.
    let reply = reply_within(&mut replies, Duration::from_millis(750)).await;
    let response = reply.response_for_test();
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["status"], "running", "{response:?}");
    assert_eq!(response.data["mode"], "pty", "{response:?}");
    assert!(reply.text_for_test().unwrap().starts_with("PTY task started:"));
    let task_id = response.data["task_id"].as_str().unwrap();
    assert_eq!(fixture.status(task_id), BgTaskStatus::Running);

    cancel.route.cancel();
    cancel.connection.cancel();
    let request: crate::protocol::RawRequest = serde_json::from_value(json!({
        "id": "pty-input",
        "command": "bash_write",
        "session_id": SESSION,
        "params": {"task_id": task_id, "input": "interactive-input\n"},
    }))
    .unwrap();
    let write = crate::commands::bash_write::handle(&request, &fixture.ctx);
    assert!(write.success, "{write:?}");
    assert_eq!(write.data["bytes_written"], 18);
    fixture.wait_terminal(task_id).await;
    assert_eq!(fixture.status(task_id), BgTaskStatus::Completed);
    assert_eq!(std::fs::read_to_string(received).unwrap(), "interactive-input");
    assert!(replies.try_recv().is_err(), "the call answers exactly once");
}

#[tokio::test]
async fn broca_worker_pty_launch_answers_at_once_and_accepts_input() {
    assert_pty_launch_hands_back("reserved:broca", "broca", "worker", true).await;
}

#[tokio::test]
async fn runner_head_pty_launch_answers_at_once_and_accepts_input() {
    assert_pty_launch_hands_back("reserved:thalamus-harness", "opencode2", "head", false).await;
}

/// The completion of a runner head's handed-back task goes to routes with
/// the same launching identity only: another route for the same session id,
/// such as the OpenCode plugin's or another runner's, never receives it.
#[tokio::test]
async fn runner_head_completion_skips_same_session_routes_of_other_identities() {
    let mut fixture = Fixture::new("runner-head-isolation");
    let route = v1_route(
        &fixture.root,
        "reserved:thalamus-harness",
        "opencode2",
        "head",
    );
    let (mut replies, _cancel) = fixture.call(
        &route,
        false,
        json!({"command": fixture.blocked_command("release"), "background": true, "timeout": 60_000}),
    );
    let task_id = launch_task_id(&reply_within(&mut replies, Duration::from_secs(15)).await);
    fixture.release("release");
    let completion = fixture.completion(&task_id).await;

    let owner = route_key(1, 1);
    let plugin = route_key(2, 1);
    let other_runner = route_key(3, 1);
    let routes = HashMap::from([
        (owner, route.clone()),
        (plugin, plugin_route(&fixture.root)),
        (
            other_runner,
            v1_route(&fixture.root, "reserved:thalamus-pi", "pi", "head"),
        ),
    ]);
    for other in [plugin, other_runner] {
        assert_eq!(routes[&other].session, route.session);
        assert_ne!(routes[&other].task_identity(), route.task_identity());
    }
    assert!(delivered_channels(&fixture.root, &routes, &completion).is_empty());
}

/// A server-owned call that is not a Thalamus runner's head keeps owning its
/// command: a `background` launch still holds the call until the command
/// ends and answers with its finished result, never a launch reply.
#[tokio::test]
async fn other_v1_carriers_and_workers_still_hold_background_launches() {
    for (principal, harness, kind, worker) in [
        // A Broca worker.
        ("reserved:broca", "broca", "worker", true),
        // A Broca head.
        ("reserved:broca", "broca", "head", false),
        // A worker on a Thalamus runner.
        ("reserved:thalamus-harness", "opencode2", "worker", true),
        // A carrier whose name only resembles a Thalamus runner.
        ("reserved:thalamus-harness-next", "opencode2", "head", false),
    ] {
        let case = format!("{principal} {harness} {kind}");
        let fixture = Fixture::new("v1-background-hold");
        let route = v1_route(&fixture.root, principal, harness, kind);
        let (mut replies, _cancel) = fixture.call(
            &route,
            worker,
            json!({"command": fixture.blocked_command("release"), "background": true, "timeout": 60_000}),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1_500), replies.recv())
                .await
                .is_err(),
            "{case}: the call must hold while its command runs"
        );
        fixture.release("release");
        let reply = reply_within(&mut replies, Duration::from_secs(30)).await;
        let response = reply.response_for_test();
        assert!(response.success, "{case}: {response:?}");
        assert_eq!(response.data["status"], "completed", "{case}: {response:?}");
        let text = reply.text_for_test().unwrap();
        assert!(
            !text.starts_with("Background task started"),
            "{case}: {text}"
        );
        assert!(text.contains("released"), "{case}: {text}");
    }
}

/// A foreground call on a Thalamus runner's head route is unchanged: it
/// holds until its command ends and answers with the command's result, the
/// same as on a Broca head route.
#[tokio::test]
async fn runner_head_foreground_call_still_holds_until_its_command_ends() {
    for principal in ["reserved:thalamus-harness", "reserved:broca"] {
        let fixture = Fixture::new("runner-head-foreground");
        let route = v1_route(&fixture.root, principal, "opencode2", "head");
        let (mut replies, _cancel) = fixture.call(
            &route,
            false,
            json!({"command": fixture.blocked_command("release"), "timeout": 60_000}),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1_500), replies.recv())
                .await
                .is_err(),
            "{principal}: a foreground call holds while its command runs"
        );
        fixture.release("release");
        let reply = reply_within(&mut replies, Duration::from_secs(30)).await;
        let response = reply.response_for_test();
        assert!(response.success, "{principal}: {response:?}");
        assert_eq!(
            response.data["status"], "completed",
            "{principal}: {response:?}"
        );
        let text = reply.text_for_test().unwrap();
        assert!(
            !text.starts_with("Background task started"),
            "{principal}: {text}"
        );
        assert!(text.contains("released"), "{principal}: {text}");
    }
}

/// A handed-back task is recorded under its head's scope, so the head's
/// confirmed scope end, or its owner stopping the session's scope, still
/// stops it.
#[tokio::test]
async fn runner_head_background_task_ends_with_its_heads_scope() {
    let fixture = Fixture::new("runner-head-scope-end");
    let route = v1_route(&fixture.root, "reserved:thalamus-pi", "pi", "head");
    let (mut replies, _cancel) = fixture.call(
        &route,
        false,
        json!({"command": fixture.blocked_command("never"), "background": true, "timeout": 60_000}),
    );
    let task_id = launch_task_id(&reply_within(&mut replies, Duration::from_secs(15)).await);
    let registry = fixture.ctx.bash_background().clone();
    let head = route.scope.clone().unwrap();
    tokio::task::spawn_blocking(move || registry.kill_scope(&head))
        .await
        .unwrap();
    let info = fixture
        .ctx
        .bash_background()
        .observed_status(&task_id, SESSION, 0)
        .unwrap()
        .info;
    assert_eq!(info.status, BgTaskStatus::Killed);
    assert_eq!(
        info.status_reason.as_deref(),
        Some(crate::bash_background::registry::SCOPE_REVOKED_REASON)
    );
}
