use super::*;
use crate::bash_background::{persistence, BgTaskStatus};
use crate::db::{access_grants as grants, remote_exec};
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
    catalog: tokio::sync::watch::Sender<Vec<String>>,
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
        let (catalog, changes) = tokio::sync::watch::channel(Vec::new());
        let (handle, ended, worker) = start_with_scope_source_impl(
            control,
            executor,
            Duration::from_millis(80),
            move || work_scopes(source_executor.clone(), watched_runs.clone()),
            Some(speech),
            Some(runs),
            changes,
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
            catalog,
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

    async fn opened(&mut self, _scope: &ScopeStamp) -> WriterFrame {
        let watch = request(&mut self.frames, "scope.watch").await;
        snapshot(&self.pending, &watch);
        turns().await;
        watch
    }

    async fn list_both_owners(&mut self, current: &ScopeStamp) {
        self.catalog
            .send_replace(vec!["prefrontal-core".into(), "prefrontal".into()]);
        self.reply(current, described(ScopeStatus::Live, true, true))
            .await;
        turns().await;
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
    ) -> Option<WriterFrame> {
        let mut reopened = None;
        let query = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let frame = self.frames.recv().await.unwrap();
                if frame.header.ty == FrameType::Cancel {
                    continue;
                }
                let body: Value = serde_json::from_slice(&frame.body).unwrap();
                if body["op"] == "scope.watch" {
                    snapshot(&self.pending, &frame);
                    reopened = Some(frame);
                    continue;
                }
                assert_eq!(body["op"], "scope.describe");
                break frame;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("missing scope.describe {stage}: {error}"));
        self.respond_to_describe(&query, expected, reply);
        turns().await;
        reopened
    }

    fn continuity_holds(&self) -> Value {
        self.metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["prefrontal_continuity_holds"]
            .clone()
    }

    async fn rehomed(&mut self, live: &ScopeStamp) -> WriterFrame {
        let watch = request(&mut self.frames, "scope.watch").await;
        let body: Value = serde_json::from_slice(&watch.body).unwrap();
        assert_eq!(
            body["scopes"][0]["scope_ref"]["owner"],
            serde_json::to_value(&live.owner).unwrap()
        );
        snapshot(&self.pending, &watch);
        turns().await;
        assert_eq!(self.status(), BgTaskStatus::Running);
        assert_eq!(self.metadata().lifecycle_scope(), Some(live));
        watch
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

#[tokio::test]
async fn prefrontal_scope_watch_holds_rename_gap_through_rollback_until_confirmed_end() {
    let old = named("prefrontal-core");
    let new = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    f.catalog.send_replace(vec!["prefrontal-core".into()]);
    let watch = f.opened(&old).await;
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
        path: crate::access_policy::policy_path(f.root.as_path()),
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

    f.catalog.send_replace(vec![]);
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
        "during the catalog gap",
        &old,
        described(ScopeStatus::Live, false, true),
    )
    .await;
    assert_survives(&f);

    f.catalog.send_replace(vec!["prefrontal".into()]);
    f.reply_through_watch_reopen(
        "after the counterpart appears",
        &old,
        described(ScopeStatus::Live, false, true),
    )
    .await;
    f.reply_through_watch_reopen(
        "against the unsynced counterpart",
        &new,
        described(ScopeStatus::NotLive, true, false),
    )
    .await;
    assert_survives(&f);
    assert_eq!(f.continuity_holds(), 1);
    assert!(f.ended.try_recv().is_err());

    f.catalog.send_replace(vec!["prefrontal-core".into()]);
    let reopened = f
        .reply_through_watch_reopen(
            "after rollback",
            &old,
            described(ScopeStatus::Live, true, true),
        )
        .await;
    assert_survives(&f);
    assert_eq!(f.continuity_holds(), 0);
    assert!(f.ended.try_recv().is_err());

    let watch = match reopened {
        Some(watch) => watch,
        None => f.opened(&old).await,
    };
    push(&f.pending, &watch, &old, ScopeWatchEvent::Ended);
    f.reply_through_watch_reopen(
        "after the real scope end",
        &old,
        described(ScopeStatus::Ended, true, true),
    )
    .await;
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
async fn prefrontal_scope_watch_continues_old_end_then_kills_new_end_despite_old_ghost() {
    let old = named("prefrontal-core");
    let new = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    let watch = f.opened(&old).await;
    f.list_both_owners(&old).await;
    push(&f.pending, &watch, &old, ScopeWatchEvent::Ended);
    f.reply(&old, described(ScopeStatus::Ended, true, true))
        .await;
    assert_eq!(f.status(), BgTaskStatus::Running);
    f.reply(&new, described(ScopeStatus::Live, true, true))
        .await;
    let watch = f.rehomed(&new).await;
    assert_eq!(f.metadata().scope.as_ref(), Some(&old));
    assert!(f.ended.try_recv().is_err());
    push(&f.pending, &watch, &new, ScopeWatchEvent::Ended);
    f.reply(&new, described(ScopeStatus::Ended, true, true))
        .await;
    f.reply(&old, described(ScopeStatus::Live, false, true))
        .await;
    f.killed().await;
}

#[tokio::test]
async fn prefrontal_scope_watch_holds_unsynced_counterpart_then_continues() {
    let old = named("prefrontal-core");
    let new = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    let watch = f.opened(&old).await;
    f.list_both_owners(&old).await;
    push(&f.pending, &watch, &old, ScopeWatchEvent::Ended);
    f.reply(&old, described(ScopeStatus::Ended, true, true))
        .await;
    f.reply(&new, described(ScopeStatus::NotLive, true, false))
        .await;
    turns().await;
    assert_eq!(f.status(), BgTaskStatus::Running);
    assert!(f.metadata().lifecycle_scope.is_none());
    assert!(f.ended.try_recv().is_err());
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), f.frames.recv())
            .await
            .unwrap()
            .unwrap();
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        if body["op"] == "scope.watch" {
            snapshot(&f.pending, &frame);
            continue;
        }
        assert_eq!(body["op"], "scope.describe");
        response(
            &f.pending,
            &frame,
            described(ScopeStatus::Ended, true, true),
        );
        break;
    }
    f.reply(&new, described(ScopeStatus::Live, true, true))
        .await;
    f.rehomed(&new).await;
    assert!(f.ended.try_recv().is_err());
}

#[tokio::test]
async fn prefrontal_scope_held_duplicate_end_still_checks_continuity_before_dedupe() {
    let old = named("prefrontal-core");
    let new = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    f.opened(&old).await;
    f.list_both_owners(&old).await;
    f.handle.observed.send(old.clone()).unwrap();
    f.reply(&old, described(ScopeStatus::Ended, true, true))
        .await;
    f.reply(&new, described(ScopeStatus::NotLive, true, false))
        .await;
    turns().await;
    assert_eq!(f.status(), BgTaskStatus::Running);
    assert!(!f
        .handle
        .completed
        .lock()
        .unwrap()
        .contains(&stamp_key(&old)));
    f.handle.observed.send(old.clone()).unwrap();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), f.frames.recv())
            .await
            .unwrap()
            .unwrap();
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        if body["op"] == "scope.watch" {
            snapshot(&f.pending, &frame);
            continue;
        }
        assert_eq!(body["op"], "scope.describe");
        assert_eq!(body["owner"], serde_json::to_value(&old.owner).unwrap());
        response(
            &f.pending,
            &frame,
            described(ScopeStatus::Ended, true, true),
        );
        break;
    }
    f.reply(&new, described(ScopeStatus::Live, true, true))
        .await;
    f.rehomed(&new).await;
    assert!(!f
        .handle
        .completed
        .lock()
        .unwrap()
        .contains(&stamp_key(&old)));
    assert!(f.ended.try_recv().is_err());
}

#[tokio::test]
async fn prefrontal_scope_watch_end_before_window_sends_no_counterpart_describe() {
    let old = named("prefrontal-core");
    let mut f = Fixture::new(&old).await;
    let watch = f.opened(&old).await;
    f.catalog.send_replace(vec!["prefrontal-core".into()]);
    push(&f.pending, &watch, &old, ScopeWatchEvent::Ended);
    f.reply(&old, described(ScopeStatus::Ended, true, true))
        .await;
    f.killed().await;
    turns().await;
    while let Ok(frame) = f.frames.try_recv() {
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_ne!(
            body["op"], "scope.describe",
            "pre-window end sent another describe: {body}"
        );
    }
}

#[tokio::test]
async fn prefrontal_scope_watch_third_owner_keeps_existing_end_behavior() {
    let other = named("third-module");
    let mut f = Fixture::new(&other).await;
    let watch = f.opened(&other).await;
    push(&f.pending, &watch, &other, ScopeWatchEvent::Ended);
    f.reply(&other, described(ScopeStatus::Ended, true, true))
        .await;
    f.killed().await;
}

#[tokio::test]
async fn prefrontal_scope_bound_counterpart_rehomes_without_an_end_signal() {
    let old = named("prefrontal-core");
    let new = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    f.opened(&old).await;
    let mut identity = test_support::route_identity(&f.root, "watch-session");
    Arc::get_mut(&mut identity.0).unwrap().scope = Some(new.clone());
    f.handle
        .routes(&HashMap::from([(route_key(71, 1), identity)]));
    f.rehomed(&new).await;
    assert_eq!(f.metadata().scope.as_ref(), Some(&old));
    assert!(f.ended.try_recv().is_err());
}

#[tokio::test]
async fn prefrontal_scope_catalog_reconciles_retired_ghost_once() {
    let old = named("prefrontal-core");
    let new = named("prefrontal");
    let mut f = Fixture::new(&old).await;
    f.opened(&old).await;
    f.catalog.send_replace(vec!["prefrontal".into()]);
    f.reply(&old, described(ScopeStatus::Live, false, true))
        .await;
    f.reply(&new, described(ScopeStatus::Live, true, true))
        .await;
    f.rehomed(&new).await;
    turns().await;
    f.catalog.send_replace(vec!["prefrontal".into()]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(frame) = f.frames.try_recv() {
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_ne!(
            body["op"], "scope.describe",
            "an unchanged catalog must not repeat the pass: {body}"
        );
    }
    assert_eq!(f.status(), BgTaskStatus::Running);
    assert!(f.ended.try_recv().is_err());
}
