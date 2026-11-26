use super::*;
use crate::subc::test_support::{route_identity, test_ctx, test_root};
use aft_engine::test_bridge::display_data::AppContextTestExt as _;

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

    fn add_route(&mut self, root: &ProjectRootId, route: RouteChannel) {
        self.live
            .insert(root.clone(), RootMeta::new(Instant::now()));
        self.epochs.insert(route.channel, route.epoch);
        insert_route_channel(
            &mut self.routes,
            &mut self.roots.channels,
            route,
            route_identity(root, &session(route)),
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
            &mut PushBuffer::default(),
            &mut SessionIdentities::default(),
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
                identity: route_identity(root, &session(route)),
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
            &mut SessionIdentities::default(),
            &mut PushBuffer::default(),
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

fn session(route: RouteChannel) -> String {
    format!("grace-session-{}", route.channel)
}

#[test]
fn rebind_queues_ack_while_watcher_observation_database_is_locked() {
    let _env = crate::test_env::hermetic_git_env_guard();
    let (_dir, root) = test_root("watcher-restore-db-fence");
    let storage = tempfile::tempdir().unwrap();
    let ctx = test_ctx();
    let route = route_key(1, 1);
    let response = configure(&ctx, &root, storage.path(), session(route));
    assert!(response.success, "{response:?}");
    let db = ctx.app().open_db(&storage.path().join("aft.db")).unwrap();
    ctx.stop_watcher_runtime();
    ctx.update_config(|config| {
        config.indexes.trigram = true;
        config.indexes.callgraph = true;
    });
    std::fs::write(root.as_path().join("seed.rs"), "fn seed() {}\n").unwrap();
    let checkout = ctx
        .data_checkout()
        .expect("test root binds to the index store");
    let aft_engine::data_layer::runtime::Fresh::Ready(before) =
        checkout.fresh_view(Duration::from_secs(60))
    else {
        panic!("initial call graph did not publish");
    };
    assert!(before.callgraph_has_file("seed.rs"));
    assert!(!before.callgraph_has_file("gap_edit.rs"));
    drop(before);
    let mut attach = Attach::new();
    assert!(attach.executor.register_actor(root.clone(), ctx.clone()));
    let mut evicted = RootMeta::new(Instant::now());
    evicted.idle_artifacts_evicted = true;
    attach.live.insert(root.clone(), evicted);
    attach.begin_bind(&root, route);

    std::thread::scope(|scope| {
        let guard = db.lock().unwrap();
        let (queued, received) = std::sync::mpsc::channel();
        let root_for_bind = root.clone();
        let ctx_for_bind = ctx.clone();
        let bind = scope.spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    attach
                        .configure_ready(&root_for_bind, route, success())
                        .await;
                    let ack_queued = !attach.writer_rx.is_empty();
                    attach.ack().await;
                    queued.send(ack_queued).unwrap();
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        aft_engine::test_bridge::display_data::context::await_watcher_gap_rescan_for_test(&ctx_for_bind),
                    )
                    .await
                    .expect("the live watcher rescan closes the gap");
                    let aft_engine::data_layer::runtime::Fresh::Ready(refreshed) =
                        checkout.fresh_view(Duration::from_secs(60))
                    else {
                        panic!("call graph did not refresh once watcher coverage recovered");
                    };
                    assert!(refreshed.callgraph_has_file("gap_edit.rs"));
                    attach.close();
                });
        });
        let before_release = received.recv_timeout(Duration::from_secs(5));
        if before_release == Ok(true) {
            assert!(ctx.watcher_gap_pending());
            std::fs::write(
                root.as_path().join("gap_edit.rs"),
                "fn after_bind_before_watch() {}\n",
            )
            .unwrap();
            let request = serde_json::from_value(json!({
                "id": "grep-watcher-gap", "command": "grep", "pattern": "after_bind_before_watch",
            }))
            .unwrap();
            let grep = crate::commands::grep::handle_grep(&request, &ctx);
            assert!(grep.success, "{:?}", grep.data);
            assert_eq!(grep.data["total_matches"], 1);
            assert!(aft_tools::tools_test_support::data_callgraph_is_building_for_test(&ctx));
        }
        // Always release the guard before joining, including when the old
        // frame loop is parked inside watcher preparation.
        drop(guard);
        bind.join().unwrap();
        assert_eq!(before_release, Ok(true));
    });
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
    ctx.mark_subc_bound();
    assert!(attach.executor.register_actor(root.clone(), ctx.clone()));
    attach.add_route(&root, first);
    let response = configure(&ctx, &root, storage.path(), session(first));
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

fn configure(
    ctx: &AppContext,
    root: &ProjectRootId,
    storage: &Path,
    session_id: String,
) -> Response {
    let req = RawRequest {
        id: "grace-bind".into(),
        command: "configure".into(),
        session_id: Some(session_id),
        lsp_hints: None,
        params: json!({
            "project_root": root.as_path(), "storage_dir": storage, "harness": "opencode",
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
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let second = route_key(2, 1);
    attach.add_route(&a, first);
    attach.add_route(&b, second);

    attach.goodbye(first, true).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.goodbye(first, true).await;
    attach.goodbye(second, true).await;
    attach.settle(false);
    assert!(!attach.live[&a].unbound_quiesced);
    assert!(!attach.live[&b].unbound_quiesced);
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.settle(false);
    assert!(attach.live[&a].goodbye_quiesce_at.is_none());
    assert!(attach.live[&a].unbound_quiesced);
    assert!(!attach.live[&b].unbound_quiesced);
    attach
        .live
        .get_mut(&a)
        .unwrap()
        .maintenance_queued_kinds
        .push_back(MaintenanceDrainKind::Lsp);
    tokio::time::advance(Duration::from_secs(30)).await;
    attach.settle(false);
    assert_eq!(
        attach.live[&a].maintenance_queued_kinds.len(),
        1,
        "a consumed grace must not quiesce again"
    );
    assert!(attach.live[&b].unbound_quiesced);
    assert!(attach.live[&b].goodbye_quiesce_at.is_none());
}

#[tokio::test(start_paused = true)]
async fn attach_goodbye_due_with_queued_ack_keeps_root_bound() {
    let (_dir, root) = test_root("goodbye-ack-race");
    let ctx = test_ctx();
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let next = route_key(2, 1);
    ctx.mark_subc_bound();
    assert!(attach.executor.register_actor(root.clone(), ctx.clone()));
    attach.add_route(&root, first);
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
    let mut attach = Attach::new();
    let first = route_key(1, 1);
    let next = route_key(2, 1);
    attach.add_route(&root, first);
    attach.goodbye(first, true).await;
    tokio::time::advance(GOODBYE_QUIESCE_GRACE - Duration::from_secs(1)).await;
    attach.begin_bind(&root, next);
    tokio::time::advance(Duration::from_secs(1)).await;
    attach.settle(false);
    assert!(
        !attach.live[&root].unbound_quiesced,
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
        attach.live[&root].unbound_quiesced,
        "failure must not wait for another tick"
    );
    assert!(attach.live[&root].goodbye_quiesce_at.is_none());
    assert!(attach.routes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn attach_shutdown_and_drain_consume_grace_without_waiting() {
    for draining in [false, true] {
        let (_dir, root) = test_root("goodbye-exit");
        let mut attach = Attach::new();
        let route = route_key(1, 1);
        attach.add_route(&root, route);
        attach.goodbye(route, true).await;
        assert!(attach.live[&root].goodbye_quiesce_at.is_some());
        assert!(!attach.live[&root].unbound_quiesced);
        if draining {
            attach.settle(true);
        } else {
            attach.close();
        }
        assert!(attach.live[&root].unbound_quiesced);
        assert!(attach.live[&root].goodbye_quiesce_at.is_none());
        tokio::time::advance(GOODBYE_QUIESCE_GRACE).await;
        attach.settle(false);
        assert!(attach.live[&root].goodbye_quiesce_at.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn attach_goodbye_burst_replay() {
    let _env = crate::test_env::hermetic_git_env_guard();
    for grace in [false, true] {
        let (_dir, root) = test_root("goodbye-burst");
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.as_path().join("fixture.rs"), "fn fixture() {}\n").unwrap();
        let ctx = test_ctx();
        ctx.set_progress_sender(Some(Arc::new(Box::new(|_| {}))));
        let mut attach = Attach::new();
        assert!(attach.executor.register_actor(root.clone(), ctx.clone()));
        let mut quiesces = 0;
        let mut cancelled_configure_tails = 0;
        let mut watcher_starts = 0;
        let mut watcher = None;
        let mut config_publications = 0;
        let mut warning_walks = 0;
        for channel in 1..=4 {
            let route = route_key(channel, 1);
            attach.begin_bind(&root, route);
            let generation = ctx.configure_generation();
            let (response, records) = crate::logging::capture_log_lines(|| {
                configure(&ctx, &root, storage.path(), session(route))
            });
            assert!(response.success, "{response:?}");
            config_publications += usize::from(ctx.configure_generation() != generation);
            warning_walks += records
                .iter()
                .filter(|line| line.contains("configure warning files walked:"))
                .count();
            attach.configure_ready(&root, route, response).await;
            attach.ack().await;
            assert!(attach.routes.contains_key(&route));
            crate::commands::configure::ensure_project_watcher(&ctx);
            let current_watcher = ctx.watcher_runtime_thread_id_for_test().expect("watcher");
            watcher_starts += usize::from(watcher != Some(current_watcher));
            watcher = Some(current_watcher);
            assert!(ctx.wait_for_project_applicability(Duration::from_secs(5)));
            let tail_was_queued = !ctx.configure_tail_idle();
            attach.goodbye(route, grace).await;
            quiesces += usize::from(attach.live[&root].unbound_quiesced);
            cancelled_configure_tails += usize::from(tail_was_queued && ctx.configure_tail_idle());
            tokio::time::advance(Duration::from_millis(250)).await;
            attach.settle(false);
        }
        let lsp_full_walks =
            aft_engine::test_bridge::search_misc::lsp::manager::applicability_full_walks_for_test(
                root.as_path(),
            );
        // The synchronous warning-walk test hook lets this replay count actual
        // disk walks in the captured records, not just pending warnings.
        let warning_walks = std::env::var_os("AFT_TEST_CONFIGURE_FORCE_SYNCHRONOUS_DEFERRED_WALK")
            .is_some()
            .then_some(warning_walks);
        println!("burst grace={grace} binds=4 goodbyes=4 quiesces={quiesces} cancelled_configure_tails={cancelled_configure_tails} watcher_starts={watcher_starts} config_publications={config_publications} warning_disk_walks={warning_walks:?} lsp_full_walks={}", lsp_full_walks);
        if grace {
            assert_eq!(quiesces, 0);
            assert_eq!(cancelled_configure_tails, 0);
            assert_eq!(watcher_starts, 1);
        } else {
            assert_eq!(quiesces, 4);
            assert!(cancelled_configure_tails > 0);
        }
        attach.close();
    }
}
