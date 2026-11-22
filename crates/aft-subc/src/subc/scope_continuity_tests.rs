use super::*;
use crate::bash_background::{persistence, BgTaskStatus};
use crate::db::access_grants as grants;
use crate::exec_remote::persistence as remote_exec;
use subc_protocol::scope::{ScopeKind, ScopeStatus};

fn named(module: &str) -> ScopeStamp {
    ScopeStamp::new(
        Principal::Reserved {
            module_id: module.into(),
        },
        "rename-agent",
        7,
        ScopeKind::Head,
    )
    .with_owner_authorized(true)
}

fn described(status: ScopeStatus, configured: bool, synced: bool) -> ModuleControlResponseToModule {
    ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch: Some(7),
        daemon_incarnation: "rename-test".into(),
        owner_configured: configured,
        owner_synced: synced,
        scope: None,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: ProjectRootId,
    ctx: Arc<AppContext>,
    handle: ScopeDescribes,
    pending: readiness::PendingControlReplies,
    frames: mpsc::Receiver<WriterFrame>,
    ended: mpsc::UnboundedReceiver<EndedScope>,
    worker: JoinHandle<()>,
    metrics: Arc<DispatchPathMetrics>,
    task: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.worker.abort();
        self.ctx.bash_background().shutdown();
    }
}

impl Fixture {
    async fn new(scope: &ScopeStamp) -> Self {
        let (dir, root) = test_support::test_root("prefrontal-scope-continuity");
        let ctx = test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("store"));
        });
        ctx.set_db(Arc::new(StdMutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        let (control, pending, frames) = control(&["scope.describe", "scope.watch"]);
        let metrics = control.metrics.clone();
        let runs = Arc::new(run_scope::RunScopes::new(control.clone()));
        let watched_runs = runs.clone();
        let source_executor = executor.clone();
        let relay = crate::gh_shim_relay::GhRelay::new(dir.path().join("unused-connection.json"));
        let speech = crate::gh_shim_speech::service::start(
            dir.path(),
            &relay,
            Arc::new(|_| Box::pin(async { Err("describe_unavailable".into()) })),
            "rename-speech-generation".into(),
            true,
        );
        let (handle, ended, worker) = start_with_scope_source_impl(
            control,
            executor,
            Duration::from_millis(80),
            move || work_scopes(source_executor.clone(), watched_runs.clone()),
            Some(speech),
            Some(runs),
        );
        let task = spawn_scoped(&ctx, &root, scope, "sleep 30");
        Self {
            _dir: dir,
            root,
            ctx,
            handle,
            pending,
            frames,
            ended,
            worker,
            metrics,
            task,
        }
    }

    fn status(&self) -> BgTaskStatus {
        self.task_status(&self.task)
    }

    fn task_status(&self, task: &str) -> BgTaskStatus {
        self.ctx
            .bash_background()
            .status(
                task,
                "watch-session",
                Some(self.root.as_path()),
                Some(&self.ctx.storage_dir()),
                0,
            )
            .unwrap()
            .info
            .status
    }

    fn metadata(&self) -> persistence::PersistedTask {
        let paths =
            persistence::task_paths(&self.ctx.storage_dir(), "watch-session", &self.task).unwrap();
        let layout = persistence::resolve_task_layout(&paths.session_dir, &self.task).unwrap();
        persistence::read_task_at(&layout).unwrap()
    }

    async fn opened(&mut self) -> WriterFrame {
        let watch = request(&mut self.frames, "scope.watch").await;
        snapshot(&self.pending, &watch);
        turns().await;
        watch
    }

    async fn reply(&mut self, expected: &ScopeStamp, reply: ModuleControlResponseToModule) {
        let query = request(&mut self.frames, "scope.describe").await;
        self.respond_to_describe(&query, expected, reply);
    }

    fn respond_to_describe(
        &self,
        query: &WriterFrame,
        expected: &ScopeStamp,
        reply: ModuleControlResponseToModule,
    ) {
        let body: Value = serde_json::from_slice(&query.body).unwrap();
        assert_eq!(
            body["owner"],
            serde_json::to_value(&expected.owner).unwrap()
        );
        assert_eq!(body["ref"], expected.scope_ref);
        response(&self.pending, query, reply);
    }

    async fn reply_through_watch_reopen(
        &mut self,
        stage: &str,
        expected: &ScopeStamp,
        reply: ModuleControlResponseToModule,
    ) {
        // Fallback polling and the watch retry can send their requests in either
        // order. Answer the reopen first: a describe reply with owner_configured
        // false cancels that watch but does not remove its queued request.
        let query = tokio::time::timeout(Duration::from_secs(2), async {
            let mut reopened = false;
            let mut query = None;
            while !reopened || query.is_none() {
                let frame = self.frames.recv().await.unwrap();
                if frame.header.ty == FrameType::Cancel {
                    continue;
                }
                let body: Value = serde_json::from_slice(&frame.body).unwrap();
                match body["op"].as_str() {
                    Some("scope.watch") => {
                        assert!(!reopened, "duplicate scope.watch {stage}");
                        snapshot(&self.pending, &frame);
                        reopened = true;
                    }
                    Some("scope.describe") => {
                        assert!(
                            query.replace(frame).is_none(),
                            "duplicate scope.describe {stage}"
                        );
                    }
                    _ => panic!("unexpected control frame {stage}: {body}"),
                }
            }
            query.unwrap()
        })
        .await
        .unwrap_or_else(|error| panic!("missing scope.watch or scope.describe {stage}: {error}"));
        self.respond_to_describe(&query, expected, reply);
        turns().await;
    }

    fn continuity_holds(&self) -> Value {
        self.metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["prefrontal_continuity_holds"]
            .clone()
    }

    async fn killed(&mut self) {
        tokio::time::timeout(Duration::from_secs(3), self.ended.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(self.status(), BgTaskStatus::Killed);
        assert_eq!(
            self.metadata().status_reason.as_deref(),
            Some("scope_revoked")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn prefrontal_scope_watch_holds_unconfigured_owner_until_confirmed_end() {
    let old = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    let watch = f.opened().await;
    let db = f.ctx.db().unwrap();
    let scope_key = grants::ScopeKey::for_grants(&old);
    let policy_key = remote_exec::PolicyKey {
        project_root: f.root.as_path().display().to_string(),
        harness: "broca".into(),
        session: "watch-session".into(),
        principal: "reserved:broca".into(),
        owner: scope_key.owner.clone(),
        scope_ref: old.scope_ref.clone(),
        epoch: old.scope_epoch,
        preset: "worker".into(),
    };
    let grant = crate::access_policy::AccessGrant {
        path: crate::policy_path::policy_path(f.root.as_path()),
        kind: crate::access_policy::GrantKind::Subtree,
        operation: crate::access_policy::AccessOperation::Read,
    };
    let launch = json!([{"path": grant.path, "mode": "read"}]);
    {
        let conn = db.lock().unwrap();
        grants::register_scope(&conn, &scope_key, None, &crate::config::Config::default()).unwrap();
        grants::materialize(&conn, &policy_key, &launch, &[(grant.clone(), None)]).unwrap();
        remote_exec::freeze(
            &conn,
            &policy_key,
            &crate::exec_remote::FrozenParams {
                remote_exec: Some(crate::exec_remote::policy::RemoteExecPolicy {
                    enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            100,
            || {},
        )
        .unwrap();
    }
    let assert_survives = |f: &Fixture| {
        assert_eq!(
            f.ctx.bash_background().with_live_scope(&old, || ()),
            Some(()),
            "the rename gap must never fence the old scope"
        );
        assert_eq!(f.status(), BgTaskStatus::Running);
        assert_eq!(f.metadata().lifecycle_scope(), Some(&old));
        let conn = db.lock().unwrap();
        assert!(!grants::scope_ended(&conn, &scope_key).unwrap());
        assert_eq!(grants::list(&conn, &scope_key).unwrap(), [grant.clone()]);
        assert_eq!(
            grants::launch_params(&conn, &policy_key).unwrap().as_ref(),
            Some(&launch)
        );
        assert!(
            remote_exec::lookup(&conn, &policy_key, 100)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
    };
    assert_survives(&f);

    push(&f.pending, &watch, &old, ScopeWatchEvent::Ended);
    f.reply(&old, described(ScopeStatus::Live, false, true))
        .await;
    turns().await;
    assert_survives(&f);
    assert_eq!(f.continuity_holds(), 1);
    assert!(f.ended.try_recv().is_err());
    let second = spawn_scoped(&f.ctx, &f.root, &old, "sleep 30");
    assert_eq!(f.task_status(&second), BgTaskStatus::Running);

    f.reply_through_watch_reopen(
        "while the owner is unconfigured",
        &old,
        described(ScopeStatus::Ended, false, true),
    )
    .await;
    assert_survives(&f);
    assert_eq!(f.continuity_holds(), 1);
    assert!(f.ended.try_recv().is_err());

    f.reply_through_watch_reopen(
        "after a configured owner confirms the end",
        &old,
        described(ScopeStatus::Ended, true, true),
    )
    .await;
    // Let the cleanup deadline use real time: database cleanup and process
    // termination run on worker threads outside the paused Tokio runtime.
    tokio::time::resume();
    f.killed().await;
    assert_eq!(f.task_status(&second), BgTaskStatus::Killed);
    assert_eq!(f.ctx.bash_background().with_live_scope(&old, || ()), None);
    let conn = db.lock().unwrap();
    assert!(grants::scope_ended(&conn, &scope_key).unwrap());
    assert!(grants::list(&conn, &scope_key).unwrap().is_empty());
    assert!(grants::launch_params(&conn, &policy_key).unwrap().is_none());
    assert!(remote_exec::lookup(&conn, &policy_key, 100)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn prefrontal_scope_watch_does_not_treat_the_old_or_unknown_owner_as_prefrontal() {
    for other in ["prefrontal-core", "third-module"] {
        let original = named(other);
        let prefrontal = named("prefrontal");
        let mut f = Fixture::new(&original).await;
        let watch = f.opened().await;
        let mut identity = test_support::route_identity(&f.root, "watch-session");
        Arc::get_mut(&mut identity.0).unwrap().scope = Some(prefrontal);
        f.handle
            .routes(&HashMap::from([(route_key(71, 1), identity)]));

        push(&f.pending, &watch, &original, ScopeWatchEvent::Ended);
        f.reply(&original, described(ScopeStatus::Ended, true, true))
            .await;
        f.killed().await;
        assert_eq!(f.metadata().lifecycle_scope(), Some(&original));
    }
}
