use super::*;
use crate::subc::test_support::{route_identity, test_ctx, test_root};

struct Attach {
    executor: Arc<Executor>,
    live: HashMap<ProjectRootId, RootMeta>,
    routes: HashMap<RouteChannel, RouteIdentity>,
    roots: RootChannels,
    pending: HashMap<RouteChannel, PendingBind>,
    epochs: HashMap<u16, u32>,
    writer: WriterSender,
    writer_rx: mpsc::Receiver<wire::WriterFrame>,
    ack_rx: mpsc::Receiver<ControlCompletion>,
    a0: A0Egress,
    metrics: Arc<DispatchPathMetrics>,
    delivery: Arc<RouteDeliverySessions>,
}

impl Attach {
    fn new() -> Self {
        let (writer, writer_rx) = mpsc::channel(32);
        let (ack_tx, ack_rx) = mpsc::channel(32);
        Self {
            a0: A0Egress::new(writer.clone(), ack_tx),
            executor: Arc::new(Executor::new()),
            live: HashMap::new(),
            routes: HashMap::new(),
            roots: RootChannels::default(),
            pending: HashMap::new(),
            epochs: HashMap::new(),
            writer,
            writer_rx,
            ack_rx,
            metrics: Arc::new(DispatchPathMetrics::new()),
            delivery: Arc::default(),
        }
    }

    fn add(&mut self, root: &ProjectRootId, ctx: &Arc<AppContext>, route: RouteChannel) {
        ctx.mark_subc_bound();
        assert!(self.executor.register_actor(root.clone(), ctx.clone()));
        self.live
            .insert(root.clone(), RootMeta::new(Instant::now()));
        self.epochs.insert(route.channel, route.epoch);
        insert_route_channel(
            &mut self.routes,
            &mut self.roots.channels,
            route,
            route_identity(root, "grace-session"),
        );
    }

    async fn goodbye(&mut self, route: RouteChannel, grace: bool) {
        self.a0.cancel(route);
        teardown_installed_route(
            &self.writer,
            None,
            None,
            &self.metrics,
            &self.executor,
            route,
            "Goodbye",
            grace,
            None,
            &mut self.epochs,
            &mut self.routes,
            &mut HashMap::new(),
            &mut self.roots,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut self.live,
            &mut HashMap::new(),
            &Arc::default(),
            &mut PendingSubcResponses::default(),
            &mut self.pending,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &Arc::new(Notify::new()),
            MAX_FRAME_BODY_LEN as usize,
            None,
            &self.delivery,
        )
        .await
        .unwrap();
    }

    fn begin_bind(&mut self, root: &ProjectRootId, route: RouteChannel) {
        self.epochs.insert(route.channel, route.epoch);
        self.a0
            .reserve(route, PROTOCOL_VERSION, 91, control_flags(), &self.metrics)
            .unwrap()
            .commit();
        self.pending.insert(
            route,
            PendingBind {
                ack_publication: None,
                bind_root_id: root.clone(),
                inserted_new_actor: false,
                cancelled: false,
                configure_request_id: "grace-bind".into(),
                started_at: Instant::now(),
                warned_half_deadline: false,
                deadline_reported: false,
                corr: 91,
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                cancellation: JobCancellation::new(),
                session: None,
            },
        );
    }

    async fn configure_ready(
        &mut self,
        root: &ProjectRootId,
        route: RouteChannel,
        response: Response,
    ) {
        self.completion(
            RouteBindCompletion {
                route,
                identity: route_identity(root, "grace-session"),
                bind_root_id: root.clone(),
                inserted_new_actor: false,
                expected_snapshot_id: None,
                applied_snapshot_id: None,
                configure_response: response,
                diagnostics_on_edit: false,
                ver: PROTOCOL_VERSION,
                corr: 91,
                flags: control_flags(),
            },
            false,
        )
        .await;
    }

    async fn ack(&mut self) {
        let mut ack = self.writer_rx.try_recv().expect("bind ACK queued");
        assert!(ack.begin_ack_write());
        ack.finish_ack_write(true);
        let ControlCompletion::AckWritten(completion) = self.ack_rx.try_recv().unwrap() else {
            panic!("expected the ACK receipt");
        };
        self.completion(completion, true).await;
    }

    async fn completion(&mut self, completion: RouteBindCompletion, ack_written: bool) {
        handle_route_bind_completion(
            &self.writer,
            completion,
            ack_written,
            &self.a0,
            &mut self.routes,
            &mut self.roots,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut self.live,
            &mut self.pending,
            &mut self.epochs,
            &self.executor,
            &Arc::new(Notify::new()),
            &self.metrics,
            None,
            &self.delivery,
        )
        .await
        .unwrap();
    }

    fn settle(&mut self, draining: bool) {
        settle_goodbye_quiesces(
            tokio::time::Instant::now(),
            draining,
            &mut self.live,
            &self.pending,
            &mut self.roots,
            &self.executor,
        );
    }

    fn close(&mut self) {
        quiesce_connection_roots(
            &mut self.live,
            &mut self.pending,
            &mut self.routes,
            &mut self.roots,
            &mut self.epochs,
            &mut HashMap::new(),
            &Arc::default(),
            &self.executor,
            &self.delivery,
            QuiescedLsp::LeaveForExitSweep,
        );
    }
}

fn success() -> Response {
    Response::success("grace-bind", json!({}))
}

#[tokio::test(start_paused = true)]
async fn attach_goodbye_rebind_preserves_configure_and_watcher() {
    let _env = crate::test_env::hermetic_git_env_guard();
    let (_dir, root) = test_root("goodbye-warm-rebind");
    let storage = tempfile::tempdir().unwrap();
    let ctx = test_ctx();
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let next = route_key(2, 1);
    attach.add(&root, &ctx, first);
    let response = configure(&ctx, &root, storage.path());
    assert!(response.success, "{response:?}");
    assert!(!ctx.configure_tail_idle());
    crate::commands::configure::ensure_project_watcher(&ctx);
    let watcher = ctx.watcher_runtime_thread_id_for_test().expect("watcher");
    let generation = ctx.configure_generation();

    attach.goodbye(first, true).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.settle(false);
    assert!(attach.routes.is_empty(), "Goodbye still removes the route");
    assert!(!ctx.subc_unbound_quiesced());
    assert!(
        !ctx.configure_tail_idle(),
        "Goodbye cancelled queued configure"
    );
    attach.begin_bind(&root, next);
    attach.configure_ready(&root, next, success()).await;
    attach.ack().await;
    tokio::time::advance(GOODBYE_QUIESCE_GRACE).await;
    attach.settle(false);

    assert_eq!(ctx.configure_generation(), generation);
    assert!(
        !ctx.configure_tail_idle(),
        "a stale deadline cancelled configure"
    );
    assert_eq!(ctx.watcher_runtime_thread_id_for_test(), Some(watcher));
    assert!(attach.routes.contains_key(&next));
    assert!(attach.live[&root].goodbye_quiesce_at.is_none());
    attach.close();
}

fn configure(ctx: &AppContext, root: &ProjectRootId, storage: &Path) -> Response {
    let req = RawRequest {
        id: "grace-bind".into(),
        command: "configure".into(),
        session_id: None,
        lsp_hints: None,
        params: json!({
            "project_root": root.as_path(), "storage_dir": storage, "harness": "opencode",
            "session_id": "grace-session",
            "config": [{"tier": "user", "source": "/user/aft.jsonc", "doc": json!({
                "indexes": {"trigram": false, "semantic": false, "callgraph": false},
            }).to_string()}],
        }),
    };
    crate::commands::configure::handle_configure(&req, ctx)
}

#[tokio::test(start_paused = true)]
async fn attach_goodbye_expiry_is_once_per_root_and_duplicate_goodbye_does_not_extend_it() {
    let (_dir_a, a) = test_root("goodbye-expiry-a");
    let (_dir_b, b) = test_root("goodbye-expiry-b");
    let (ctx_a, ctx_b) = (test_ctx(), test_ctx());
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let second = route_key(2, 1);
    attach.add(&a, &ctx_a, first);
    attach.add(&b, &ctx_b, second);
    let generation = ctx_a.configure_generation();

    attach.goodbye(first, true).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.goodbye(first, true).await;
    attach.goodbye(second, true).await;
    attach.settle(false);
    assert!(!ctx_a.subc_unbound_quiesced());
    assert!(!ctx_b.subc_unbound_quiesced());
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.settle(false);
    assert!(ctx_a.subc_unbound_quiesced());
    assert!(attach.live[&a].goodbye_quiesce_at.is_none());
    assert!(attach.live[&a].unbound_quiesced);
    assert!(!ctx_b.subc_unbound_quiesced());
    assert_eq!(ctx_a.configure_generation(), generation + 1);
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.settle(false);
    assert_eq!(ctx_a.configure_generation(), generation + 1);
    assert!(ctx_b.subc_unbound_quiesced());
    assert!(attach.live[&b].goodbye_quiesce_at.is_none());
}

#[tokio::test(start_paused = true)]
async fn attach_goodbye_due_with_queued_ack_keeps_root_bound() {
    let (_dir, root) = test_root("goodbye-ack-race");
    let ctx = test_ctx();
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let next = route_key(2, 1);
    attach.add(&root, &ctx, first);
    let generation = ctx.configure_generation();
    attach.goodbye(first, true).await;
    tokio::time::advance(GOODBYE_QUIESCE_GRACE - Duration::from_secs(1)).await;
    attach.begin_bind(&root, next);
    attach.configure_ready(&root, next, success()).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    attach.settle(false);
    assert!(attach.routes.is_empty());
    assert!(attach.live[&root].goodbye_quiesce_at.is_some());
    assert!(!ctx.subc_unbound_quiesced());
    attach.ack().await;
    attach.settle(false);
    assert!(attach.routes.contains_key(&next));
    assert!(attach.live[&root].goodbye_quiesce_at.is_none());
    assert_eq!(ctx.configure_generation(), generation);
    assert!(!ctx.subc_unbound_quiesced());

    // A later Goodbye replaces the cancelled deadline with a fresh window.
    attach.goodbye(next, true).await;
    attach.settle(false);
    assert_eq!(ctx.configure_generation(), generation);
    tokio::time::advance(GOODBYE_QUIESCE_GRACE).await;
    attach.settle(false);
    assert_eq!(ctx.configure_generation(), generation + 1);
}

#[tokio::test(start_paused = true)]
async fn attach_goodbye_due_while_bind_pending_quiesces_as_soon_as_bind_fails() {
    let (_dir, root) = test_root("goodbye-bind-failure-race");
    let ctx = test_ctx();
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let next = route_key(2, 1);
    attach.add(&root, &ctx, first);
    attach.goodbye(first, true).await;
    tokio::time::advance(GOODBYE_QUIESCE_GRACE - Duration::from_secs(1)).await;
    attach.begin_bind(&root, next);
    tokio::time::advance(Duration::from_secs(1)).await;
    attach.settle(false);
    assert!(
        !ctx.subc_unbound_quiesced(),
        "a pending bind protects ACK publication"
    );
    attach
        .configure_ready(
            &root,
            next,
            Response::error("grace-bind", "invalid_config", "bad config"),
        )
        .await;
    assert!(
        ctx.subc_unbound_quiesced(),
        "failure must not wait for another tick"
    );
    assert!(attach.live[&root].goodbye_quiesce_at.is_none());
    assert!(attach.routes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn attach_shutdown_and_drain_consume_grace_without_waiting() {
    for draining in [false, true] {
        let (_dir, root) = test_root("goodbye-exit");
        let ctx = test_ctx();
        let mut attach = Attach::new();
        let route = route_key(1, 1);
        attach.add(&root, &ctx, route);
        attach.goodbye(route, true).await;
        assert!(attach.live[&root].goodbye_quiesce_at.is_some());
        assert!(!ctx.subc_unbound_quiesced());
        if draining {
            attach.settle(true);
        } else {
            attach.close();
        }
        assert!(ctx.subc_unbound_quiesced());
        assert!(attach.live[&root].goodbye_quiesce_at.is_none());
        let generation = ctx.configure_generation();
        tokio::time::advance(GOODBYE_QUIESCE_GRACE).await;
        attach.settle(false);
        assert_eq!(ctx.configure_generation(), generation);
    }
}
