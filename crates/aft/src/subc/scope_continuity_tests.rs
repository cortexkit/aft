use super::*;
use crate::bash_background::{persistence, BgTaskStatus};
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
            task,
        }
    }

    fn status(&self) -> BgTaskStatus {
        self.ctx
            .bash_background()
            .status(
                &self.task,
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

    async fn opened(&mut self, scope: &ScopeStamp) -> WriterFrame {
        let watch = request(&mut self.frames, "scope.watch").await;
        snapshot(&self.pending, &watch);
        self.reply(scope, described(ScopeStatus::Live, true, true))
            .await;
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
        let body: Value = serde_json::from_slice(&query.body).unwrap();
        assert_eq!(
            body["owner"],
            serde_json::to_value(&expected.owner).unwrap()
        );
        assert_eq!(body["ref"], expected.scope_ref);
        response(&self.pending, &query, reply);
    }

    async fn rehomed(&mut self, live: &ScopeStamp) -> WriterFrame {
        let watch = request(&mut self.frames, "scope.watch").await;
        let body: Value = serde_json::from_slice(&watch.body).unwrap();
        assert_eq!(
            body["scopes"][0]["scope_ref"]["owner"],
            serde_json::to_value(&live.owner).unwrap()
        );
        snapshot(&self.pending, &watch);
        self.reply(live, described(ScopeStatus::Live, true, true))
            .await;
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
