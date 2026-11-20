use super::*;
use codemode_owners::{admit_frame, FrameAdmission};
use subc_protocol::{
    scope::ScopeStatus,
    session::{ModuleControlRequestFromModule, ModuleControlResponseToModule},
};

const NO_HEAD: &str = "no live route of the run's head scope is bound to AFT";

struct Restart {
    _storage: tempfile::TempDir,
    _workspaces: [tempfile::TempDir; 2],
    runner: RouteIdentity,
    routes: HashMap<RouteChannel, RouteIdentity>,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    daemon: mpsc::Receiver<WriterFrame>,
    replies: readiness::PendingControlReplies,
    runs: run_scope::RunScopes,
    waits: persistence::DatabaseWaits,
    corr: u64,
}

fn context(storage: &Path) -> Arc<AppContext> {
    let ctx = test_support::test_ctx();
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    ctx.finish_database_runtime(Ok(()));
    ctx
}

impl Restart {
    fn new(
        worker: bool,
        same_root: bool,
        before_restart: impl FnOnce(&AppContext, &RouteIdentity),
    ) -> Self {
        let (runner_dir, runner_root) = test_root("codemode-restart-runner");
        let (other_dir, other_root) = test_root("codemode-restart-other");
        let (runner_stamp, run_stamp) = if worker {
            (
                worker_stamp("worker-a"),
                worker_run_stamp("worker-a", 5, Some("linked")),
            )
        } else {
            (
                head_stamp("head-a", "agent-a"),
                run_stamp("agent-a", "head-a", Some("linked")),
            )
        };
        let mut runner = route(
            &runner_root,
            "runner-session",
            runner_stamp,
            CallMode::Classic,
        );
        let data = Arc::get_mut(&mut runner.0).unwrap();
        data.harness = "opencode".to_owned();
        data.consumer_elicitation_capable = true;
        data.project_root = runner_root.as_path().join("workspace");
        std::fs::create_dir(&data.project_root).unwrap();
        data.spawn_principal = AuthenticatedPrincipal::RouteBind {
            trust: data.trust.sandbox_trust(),
            route_channel: 1,
            route_epoch: 4,
            project_root: data.project_root.clone(),
            harness: data.harness.clone(),
            session_id: data.session.clone(),
            principal_id: Some("direct".into()),
        };

        let storage = tempfile::tempdir().unwrap();
        {
            let before = context(storage.path());
            codemode_owners::save(&before, &runner);
            before_restart(&before, &runner);
        }
        let (writer, daemon) = mpsc::channel(8);
        let replies = readiness::PendingControlReplies::default();
        let runs = run_scope::RunScopes::new(Arc::new(readiness::DaemonControl::new(
            writer,
            Arc::new(DispatchPathMetrics::new()),
            PROTOCOL_VERSION,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
            vec![],
            replies.clone(),
            200,
        )));
        let run_root = if same_root { runner_root } else { other_root };
        let run = route(&run_root, "run-session", run_stamp, CallMode::Codemode);
        runs.bind(&run).unwrap();
        let executor = Arc::new(Executor::new());
        let ctx = context(storage.path());
        assert!(executor.register_actor(run_root, ctx.clone()));
        Self {
            _storage: storage,
            _workspaces: [runner_dir, other_dir],
            runner,
            routes: HashMap::from([(channel(9), run)]),
            ctx,
            executor,
            daemon,
            replies,
            runs,
            waits: Default::default(),
            corr: 1,
        }
    }

    fn start(&mut self) -> FrameAdmission {
        let decoded = DecodedFrame {
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                9,
                1,
                self.corr,
                serde_json::to_vec(&call("read", json!({"path":"notes.txt"}))).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
        };
        self.corr += 1;
        self.admit(decoded)
    }

    fn admit(&mut self, decoded: DecodedFrame) -> FrameAdmission {
        admit_frame(
            &self.routes,
            channel(9),
            decoded,
            &mut self.waits,
            &self.executor,
        )
    }

    async fn request(&mut self) -> WriterFrame {
        let request = tokio::time::timeout(Duration::from_secs(5), self.daemon.recv())
            .await
            .expect("saved runner admission must describe the parent")
            .unwrap();
        let ModuleControlRequestFromModule::ScopeDescribe { owner, scope_ref } =
            serde_json::from_slice(&request.body).unwrap()
        else {
            panic!("expected scope.describe request");
        };
        let scope = self.runner.scope.as_ref().unwrap();
        assert_eq!(scope_ref, scope.scope_ref);
        assert_eq!(owner, scope.owner);
        request
    }

    fn answer(&self, request: WriterFrame, status: ScopeStatus, epoch: Option<u64>) {
        let reply = ModuleControlResponseToModule::ScopeDescribe {
            status,
            scope_epoch: epoch,
            daemon_incarnation: "restart".into(),
            owner_synced: true,
            owner_configured: true,
            scope: (status == ScopeStatus::Live).then(|| {
                let mut scope = self.runner.scope.clone().unwrap();
                scope.scope_epoch = epoch.unwrap();
                scope
            }),
        };
        assert!(self.replies.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&reply).unwrap(),
            )
            .unwrap()
        ));
    }

    async fn finish(&mut self, mut admission: FrameAdmission) -> FrameAdmission {
        for _ in 0..3 {
            if !matches!(admission, FrameAdmission::Deferred) {
                return admission;
            }
            let decoded = tokio::time::timeout(Duration::from_secs(12), self.waits.next())
                .await
                .expect("saved admission did not finish");
            admission = self.admit(decoded);
        }
        assert!(
            matches!(
                admission,
                FrameAdmission::Ready(..) | FrameAdmission::Refused(..)
            ),
            "saved admission did not stabilize"
        );
        admission
    }

    async fn live(&mut self) -> FrameAdmission {
        let pending = self.start();
        assert!(matches!(pending, FrameAdmission::Deferred));
        let request = self.request().await;
        self.answer(
            request,
            ScopeStatus::Live,
            Some(self.runner.scope.as_ref().unwrap().scope_epoch),
        );
        self.finish(pending).await
    }
}

fn effective(admission: FrameAdmission) -> RouteIdentity {
    match admission {
        FrameAdmission::Ready(_, Some(identity)) => identity,
        FrameAdmission::Refused(_, error) => panic!("unexpected refusal: {error:?}"),
        _ => panic!("expected admitted code-mode call"),
    }
}

fn error(admission: FrameAdmission) -> ErrorBody {
    match admission {
        FrameAdmission::Refused(_, error) => error,
        _ => panic!("expected code-mode refusal"),
    }
}

fn assert_no_head(admission: FrameAdmission) {
    let error = error(admission);
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    assert_eq!(error.message, NO_HEAD);
    assert_eq!(
        error.detail.unwrap(),
        json!({"reason":"head_route_not_bound", "agent_id":"agent-a", "run_id":"run-1"})
    );
}

fn rebind(runner: &RouteIdentity, session: &str, disabled: &str) -> RouteIdentity {
    let mut updated = route(
        &runner.root,
        session,
        runner.scope.clone().unwrap(),
        CallMode::Classic,
    );
    let data = Arc::get_mut(&mut updated.0).unwrap();
    data.harness = runner.harness.clone();
    data.project_root = runner.project_root.clone();
    data.consumer_elicitation_capable = runner.consumer_elicitation_capable;
    data.trust = runner.trust;
    data.spawn_principal = runner.spawn_principal.clone();
    if let AuthenticatedPrincipal::RouteBind {
        route_channel,
        route_epoch,
        session_id,
        ..
    } = &mut data.spawn_principal
    {
        *route_channel += 1;
        *route_epoch += 1;
        *session_id = session.into();
    }
    data.disabled_tools = Arc::new(vec![disabled.into()]);
    updated
}

#[tokio::test]
async fn codemode_restart_inherits_exact_runner_access_and_describes_once() {
    let mut f = Restart::new(false, true, |_, _| {});
    let recovered = effective(f.live().await);
    f.routes.insert(channel(1), f.runner.clone());
    let live = effective(f.start());
    f.routes.remove(&channel(1));
    assert!(head_access_differences(&recovered, &live).is_empty());
    assert_eq!(recovered.spawn_principal, live.spawn_principal);
    assert_eq!(recovered.scope, live.scope);
    assert_eq!(
        recovered.run_owner.as_ref().unwrap().owner_route.scope,
        f.runner.scope
    );
    assert_eq!(
        recovered.run_owner.as_ref().unwrap().owner_route_channel,
        None
    );
    let pending = f.start();
    assert!(head_access_differences(&effective(f.finish(pending).await), &live).is_empty());
    assert!(
        f.daemon.try_recv().is_err(),
        "subsequent calls reuse the parent proof"
    );
}

#[tokio::test]
async fn codemode_restart_refuses_ended_not_live_new_epoch_and_timeout_without_new_text() {
    for answer in [
        Some((ScopeStatus::Ended, Some(3))),
        Some((ScopeStatus::NotLive, None)),
        Some((ScopeStatus::Live, Some(4))),
        None,
    ] {
        let mut f = Restart::new(false, true, |_, _| {});
        let pending = f.start();
        let request = f.request().await;
        if let Some((status, epoch)) = answer {
            f.answer(request, status, epoch);
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(11)).await;
            tokio::time::resume();
        }
        assert_no_head(f.finish(pending).await);
        let pending = f.start();
        assert_no_head(f.finish(pending).await);
        assert!(
            f.daemon.try_recv().is_err(),
            "negative proof is cached for the run"
        );
    }
}

#[tokio::test]
async fn codemode_restart_live_route_wins_even_after_saved_lookup_started() {
    let mut f = Restart::new(false, true, |_, _| {});
    let live = rebind(&f.runner, "live-runner", "only-disabled-on-live");
    let pending = f.start();
    let request = f.request().await;
    f.routes.insert(channel(2), live.clone());
    f.answer(request, ScopeStatus::Live, Some(3));
    let admitted = effective(f.finish(pending).await);
    assert_eq!(
        admitted.run_owner.as_ref().unwrap().owner_route_channel,
        Some(channel(2))
    );
    assert!(head_access_differences(&admitted, &live).is_empty());
    assert!(head_access_differences(&effective(f.start()), &live).is_empty());
    assert!(f.daemon.try_recv().is_err());
}

#[tokio::test]
async fn codemode_restart_head_lease_does_not_shadow_the_saved_runner() {
    let mut f = Restart::new(false, true, |_, _| {});
    let mut lease = rebind(&f.runner, "head-scope-owner", "not-a-runner");
    let data = Arc::get_mut(&mut lease.0).unwrap();
    if let AuthenticatedPrincipal::RouteBind {
        principal_id: principal,
        ..
    } = &mut data.spawn_principal
    {
        *principal = principal_id(&Some(data.scope.as_ref().unwrap().owner.clone()));
    }
    assert!(is_scope_owner_route(&lease));
    codemode_owners::save(&f.ctx, &lease);
    f.routes.insert(channel(2), lease);

    let admitted = effective(f.live().await);
    assert!(head_access_differences(&admitted, &f.runner).is_empty());
    assert_eq!(admitted.spawn_principal, f.runner.spawn_principal);
    assert_eq!(
        admitted.run_owner.as_ref().unwrap().owner_route_channel,
        None
    );
    let db = f.ctx.db().unwrap();
    let rows: i64 = db
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM codemode_owners", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "the head scope owner's bind must not be saved");
}

#[tokio::test]
async fn codemode_restart_worker_uses_its_separately_saved_policy() {
    let mut f = Restart::new(true, true, |ctx, runner| {
        save_policy(ctx, runner, json!({"confine":"root"}));
    });
    let recovered = effective(f.live().await);
    f.routes.insert(channel(1), f.runner.clone());
    let live = f.start();
    let live = effective(f.finish(live).await);
    assert_eq!(
        remote_policy::key(&recovered, Some("worker")),
        remote_policy::key(&live, Some("worker"))
    );
    assert!(remote_policy::confined(&f.ctx, &live).unwrap());
    assert!(remote_policy::confined(&f.ctx, &recovered).unwrap());
}

#[tokio::test]
async fn codemode_restart_confirmed_end_deletes_snapshot_and_refuses_next_call() {
    let mut f = Restart::new(false, true, |_, _| {});
    effective(f.live().await);
    let stamp = f.runner.scope.clone().unwrap();
    let metrics = Arc::new(DispatchPathMetrics::new());
    scope_describe::accept_confirmed_end(&f.executor, Some(&f.runs), &metrics, &stamp);
    let (notices, mut ended) = mpsc::unbounded_channel();
    let cleanup = tokio::time::timeout(
        Duration::from_secs(5),
        scope_describe::finish_confirmed_end(f.executor.clone(), stamp, metrics, notices),
    )
    .await
    .expect("confirmed end cleanup did not finish");
    assert!(cleanup.is_none());
    assert!(ended.try_recv().is_ok());
    let db = f.ctx.db().unwrap();
    let count: i64 = db
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM codemode_owners", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "confirmed end must remove the stored row, not just fence reads"
    );
    assert_no_head(f.start());
    assert!(f.daemon.try_recv().is_err());
}

#[tokio::test]
async fn codemode_restart_rebind_replaces_the_same_stable_runner_identity() {
    let mut f = Restart::new(false, true, |ctx, runner| {
        codemode_owners::save(ctx, &rebind(runner, &runner.session, "new-disabled"));
    });
    let admitted = effective(f.live().await);
    assert_eq!(&**admitted.disabled_tools, &["new-disabled"]);
    let db = f.ctx.db().unwrap();
    let count: i64 = db
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM codemode_owners", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn codemode_restart_newer_scope_lease_evicts_old_runner_but_never_supplies_access() {
    let mut f = Restart::new(true, true, |ctx, runner| {
        let mut lease = rebind(runner, "new-epoch-lease", "not-a-runner");
        let data = Arc::get_mut(&mut lease.0).unwrap();
        let stamp = data.scope.as_mut().unwrap();
        stamp.scope_epoch += 1;
        if let AuthenticatedPrincipal::RouteBind {
            principal_id: principal,
            ..
        } = &mut data.spawn_principal
        {
            *principal = principal_id(&Some(stamp.owner.clone()));
        }
        access_grants::register(&lease, ctx).unwrap();
        codemode_owners::save(ctx, &lease);
        // Even a late completion of an old-epoch bind cannot revive the row.
        codemode_owners::save(ctx, runner);
        let db = ctx.db().unwrap();
        let rows: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM codemode_owners", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    });
    let pending = f.start();
    let refusal = error(f.finish(pending).await);
    assert_eq!(refusal.code, RUN_OWNER_NOT_BOUND);
    assert_eq!(
        refusal.message,
        "no live runner route of the worker scope is bound to AFT yet"
    );
    assert_eq!(refusal.detail.unwrap()["reason"], "worker_route_not_bound");
    assert!(f.daemon.try_recv().is_err());
}

#[tokio::test]
async fn codemode_restart_does_not_start_an_inactive_owner_root() {
    let mut f = Restart::new(false, false, |_, _| {});
    assert!(!f.executor.actor_registered(&f.runner.root));
    assert_no_head(f.live().await);
    assert!(!f.executor.actor_registered(&f.runner.root));
}

#[tokio::test]
async fn codemode_restart_distinct_saved_runners_disagree_with_existing_ambiguity() {
    let mut f = Restart::new(false, true, |ctx, runner| {
        codemode_owners::save(
            ctx,
            &rebind(runner, "another-runner", &runner.disabled_tools[0]),
        );
    });
    let refusal = error(f.live().await);
    let detail = refusal.detail.unwrap();
    assert_eq!(detail["reason"], "head_route_ambiguous");
    assert_eq!(detail["fields"], json!(["session", "spawn_principal"]));
}
