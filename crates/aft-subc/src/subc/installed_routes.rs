//! Installed-route bookkeeping, including per-runner status key-value lifetimes.
use super::*;
use crate::subc_config::BindConfigCache;

#[derive(Default)]
pub(super) struct RootChannels {
    pub(super) channels: HashMap<ProjectRootId, HashSet<RouteChannel>>,
    pub(super) config: BindConfigCache,
}

impl std::ops::Deref for RootChannels {
    type Target = HashMap<ProjectRootId, HashSet<RouteChannel>>;

    fn deref(&self) -> &Self::Target {
        &self.channels
    }
}

impl RootChannels {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn remove(&mut self, root: &ProjectRootId) {
        self.channels.remove(root);
        self.config.release_project(root.as_path());
    }

    pub(super) fn release_if_unbound(
        &mut self,
        root: &ProjectRootId,
        pending_binds: &HashMap<RouteChannel, PendingBind>,
        replacement_root: Option<&ProjectRootId>,
    ) -> bool {
        let has_pending_bind = pending_binds
            .values()
            .any(|bind| &bind.bind_root_id == root);
        if should_quiesce_removed_root(root, self, has_pending_bind, replacement_root) {
            self.config.release_project(root.as_path());
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
impl From<HashMap<ProjectRootId, HashSet<RouteChannel>>> for RootChannels {
    fn from(channels: HashMap<ProjectRootId, HashSet<RouteChannel>>) -> Self {
        Self {
            channels,
            ..Self::default()
        }
    }
}

pub(super) fn remove_root_channel(
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    channel: RouteChannel,
) {
    if let Some(channels) = root_channels.get_mut(root) {
        channels.remove(&channel);
        if channels.is_empty() {
            root_channels.remove(root);
        }
    }
}

pub(super) fn status_unbound(route: &RouteIdentity) {
    crate::late_result_bus::status::route_unbound(Arc::as_ptr(&route.0) as usize, &route.session);
}

pub(super) fn remove_route_channel(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
) -> Option<RouteIdentity> {
    let removed = routes.remove(&channel);
    if let Some(identity) = &removed {
        status_unbound(identity);
        runner_notices::route_unbound(identity);
        remove_root_channel(root_channels, &identity.root, channel);
        release_semantic_opener_if_unrouted(routes, identity);
    }
    removed
}

pub(super) fn insert_route_channel(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
    identity: RouteIdentity,
) {
    if crate::semantic_admission::harness_opens_root(&identity.harness) {
        crate::semantic_admission::note_opened(&identity.session, identity.root.as_path());
    }
    crate::late_result_bus::status::route_bound(
        Arc::as_ptr(&identity.0) as usize,
        &identity.session,
    );
    if let Some(previous) = routes.insert(channel, identity.clone()) {
        status_unbound(&previous);
        runner_notices::route_unbound(&previous);
        remove_root_channel(root_channels, &previous.root, channel);
        release_semantic_opener_if_unrouted(routes, &previous);
    }
    root_channels
        .entry(identity.root.clone())
        .or_default()
        .insert(channel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subc::test_support::{
        route_identity, route_identity_with_harness, test_ctx, test_root,
    };
    use aft_engine::data_layer::meaning_query::DataSemantic;

    fn meaning_route(root: &ProjectRootId, harness: &str, scope: Option<&str>) -> RouteIdentity {
        let mut route = route_identity_with_harness(
            root,
            "alfonso:head/meaning",
            BindTrust::FirstParty,
            harness,
        );
        Arc::get_mut(&mut route.0).unwrap().scope = scope.map(|kind| {
            serde_json::from_value(json!({
                "owner": {"kind":"reserved", "module_id":"prefrontal-core"},
                "ref": format!("meaning-{kind}"), "scope_epoch": 1, "kind": kind,
                "owner_authorized": true,
            }))
            .unwrap()
        });
        route
    }

    fn meaning_context(root: &ProjectRootId) -> Arc<AppContext> {
        let ctx = test_ctx();
        ctx.set_canonical_cache_root(root.as_path().to_path_buf());
        ctx.update_config(|config| {
            config.project_root = Some(root.as_path().to_path_buf());
            config.indexes.semantic = true;
            config.indexes.trigram = true;
            config.indexes.callgraph = false;
        });
        ctx
    }

    fn wait_for_meaning_vectors(ctx: &AppContext) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = DataSemantic::probe(ctx);
            if matches!(&status, Some(DataSemantic::Serving(meaning)) if meaning.dimension() == Some(2))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "meaning did not fill: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn meaning_hosted_heads_fill_on_bind() {
        use crate::semantic_admission::{admission, SemanticAdmission};
        use aft_engine::test_bridge::display_data::meaning::fill_with_constant_vectors_for_test;

        for harness in ["opencode2", "claude-code", "broca"] {
            let (_dir, root) = test_root("meaning-hosted-head");
            std::fs::write(
                root.as_path().join("lib.rs"),
                "pub fn needle() -> u32 { 1 }\n",
            )
            .unwrap();
            let ctx = meaning_context(&root);
            let _model = fill_with_constant_vectors_for_test(root.as_path(), 2);
            let mut routes = HashMap::new();
            let mut roots = HashMap::new();
            let channel = route_key(61, 1);
            insert_route_channel(
                &mut routes,
                &mut roots,
                channel,
                meaning_route(&root, harness, Some("head")),
            );
            assert_eq!(
                admission(root.as_path(), &[]),
                SemanticAdmission::InteractiveSession,
                "{harness}"
            );
            aft_engine::data_layer::runtime::root_bound(ctx.as_ref());
            wait_for_meaning_vectors(&ctx);
            remove_route_channel(&mut routes, &mut roots, channel);
        }
    }

    #[test]
    fn meaning_only_qualifying_routes_hold_the_root_open() {
        use crate::semantic_admission::{admission, SemanticAdmission::*};

        let (_dir, root) = test_root("meaning-route-lifetime");
        let mut routes = HashMap::new();
        let mut roots = HashMap::new();
        let plugin = route_key(61, 1);
        for (harness, scope, expected) in [
            ("opencode", None, InteractiveSession),
            ("pi", None, InteractiveSession),
            ("opencode2", None, NotOpened),
            ("claude-code", None, NotOpened),
            ("broca", None, NotOpened),
            ("opencode", Some("worker"), NotOpened),
            ("pi", Some("ephemeral"), NotOpened),
        ] {
            insert_route_channel(
                &mut routes,
                &mut roots,
                plugin,
                meaning_route(&root, harness, scope),
            );
            let observed = admission(root.as_path(), &[]);
            remove_route_channel(&mut routes, &mut roots, plugin);
            assert_eq!(observed, expected, "{harness} {scope:?}");
        }

        let first = route_key(62, 1);
        let second = route_key(63, 1);
        let worker = route_key(64, 1);
        for (channel, scope) in [(first, "head"), (second, "head"), (worker, "worker")] {
            insert_route_channel(
                &mut routes,
                &mut roots,
                channel,
                meaning_route(&root, "broca", Some(scope)),
            );
        }
        remove_route_channel(&mut routes, &mut roots, first);
        assert_eq!(admission(root.as_path(), &[]), InteractiveSession);
        remove_route_channel(&mut routes, &mut roots, second);
        assert_eq!(
            admission(root.as_path(), &[]),
            NotOpened,
            "a worker with the same session does not hold the opener"
        );
        remove_route_channel(&mut routes, &mut roots, worker);
    }

    #[test]
    fn meaning_already_bound_worker_fills_on_query_after_its_git_family_opens() {
        use crate::semantic_admission::{admission, SemanticAdmission};
        use aft_engine::test_bridge::display_data::meaning::fill_with_constant_vectors_for_test;

        let (_head_dir, head) = test_root("meaning-family-head");
        let (_worker_dir, worker) = test_root("meaning-family-worker");
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .current_dir(head.as_path())
                .args(args)
                .output()
                .unwrap();
            assert!(result.status.success(), "git {args:?}: {result:?}");
        };
        git(&["init"]);
        std::fs::write(
            head.as_path().join("lib.rs"),
            "pub fn needle() -> u32 { 1 }\n",
        )
        .unwrap();
        git(&["add", "lib.rs"]);
        git(&[
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture",
        ]);
        git(&[
            "worktree",
            "add",
            "--detach",
            worker.as_path().to_str().unwrap(),
            "HEAD",
        ]);

        let ctx = meaning_context(&worker);
        let _model = fill_with_constant_vectors_for_test(worker.as_path(), 2);
        let mut routes = HashMap::new();
        let mut roots = HashMap::new();
        let worker_channel = route_key(61, 1);
        let head_channel = route_key(62, 1);
        insert_route_channel(
            &mut routes,
            &mut roots,
            worker_channel,
            meaning_route(&worker, "broca", Some("worker")),
        );
        aft_engine::data_layer::runtime::root_bound(ctx.as_ref());
        assert!(
            DataSemantic::current(ctx.as_ref()).is_none(),
            "a worker alone stays off"
        );

        insert_route_channel(
            &mut routes,
            &mut roots,
            head_channel,
            meaning_route(&head, "opencode", Some("head")),
        );
        assert_eq!(
            admission(worker.as_path(), &[]),
            SemanticAdmission::OpenedRepository
        );
        let status = DataSemantic::current(ctx.as_ref());
        assert!(
            status.is_some(),
            "the next query must refresh the cached off configuration"
        );
        wait_for_meaning_vectors(&ctx);
        remove_route_channel(&mut routes, &mut roots, head_channel);
        assert_eq!(
            admission(worker.as_path(), &[]),
            SemanticAdmission::NotOpened
        );
        remove_route_channel(&mut routes, &mut roots, worker_channel);
    }

    #[test]
    fn parsed_project_config_leaves_with_last_route_but_survives_replacement() {
        use crate::{harness::Harness, subc_config::PreparedBindConfig};
        let (_dir, root) = test_root("route-config-lifetime");
        let path = root.as_path().join(".cortexkit/aft.jsonc");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_600_000_000, 0))
            .unwrap();
        let mut routes = HashMap::new();
        let mut roots = RootChannels::default();
        let first = route_key(71, 1);
        let second = route_key(72, 1);
        let bind = |roots: &mut RootChannels| {
            PreparedBindConfig::read_cached(
                &mut roots.config,
                None,
                None,
                root.as_path(),
                root.as_path(),
                &Harness::Opencode,
            )
            .unwrap();
        };
        bind(&mut roots);
        for route in [first, second] {
            insert_route_channel(
                &mut routes,
                &mut roots.channels,
                route,
                route_identity(&root, "config-lifetime"),
            );
        }
        remove_route_channel(&mut routes, &mut roots.channels, first);
        assert!(!roots.release_if_unbound(&root, &HashMap::new(), None));
        bind(&mut roots);
        remove_route_channel(&mut routes, &mut roots.channels, second);
        assert!(!roots.release_if_unbound(&root, &HashMap::new(), Some(&root)));
        bind(&mut roots);
        assert_eq!(roots.config.file_parses(), 1);
        assert!(roots.release_if_unbound(&root, &HashMap::new(), None));
        bind(&mut roots);
        assert_eq!(roots.config.file_parses(), 2);
    }

    #[test]
    fn status_kv_tracks_installed_routes_and_channel_replacement() {
        let (_dir, root) = test_root("status-kv-routes");
        let session = "status-kv:installed/runner";
        let other = "status-kv:replacement/runner";
        let mut routes = HashMap::new();
        let mut roots = HashMap::new();
        let first = route_key(51, 1);
        let second = route_key(52, 1);
        insert_route_channel(
            &mut routes,
            &mut roots,
            first,
            route_identity(&root, session),
        );
        insert_route_channel(
            &mut routes,
            &mut roots,
            second,
            route_identity(&root, session),
        );
        let client = crate::fleet_status::FleetStatusClient::dial_channel(4).0;
        client.publish(root.as_path(), "opencode", session, "[AFT E0 W0 | T1]");
        assert_eq!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(session)
                .unwrap()["segment"],
            "[AFT E0 W0 | T1]"
        );
        remove_route_channel(&mut routes, &mut roots, first);
        assert!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(session)
                .is_some()
        );
        insert_route_channel(
            &mut routes,
            &mut roots,
            second,
            route_identity(&root, other),
        );
        assert!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(session)
                .is_none(),
            "replacement ends the last old-session route"
        );
        assert!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(other)
                .is_none(),
            "a bound route alone does not publish a segment"
        );
        client.publish(root.as_path(), "opencode", session, "late completion");
        assert!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(session)
                .is_none()
        );
        client.publish(root.as_path(), "opencode", other, "[AFT E1 W0 | T2]");
        assert!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(other)
                .is_some()
        );
        remove_route_channel(&mut routes, &mut roots, second);
        assert!(
            aft_engine::test_bridge::display_data::late_result_bus::status::test_value(other)
                .is_none()
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn process_route_bind_completion(
    writer_tx: &WriterSender,
    completion: ControlCompletion,
    a0: &A0Egress,
    on_management_install: impl FnOnce(&ManagementIdentity),
    management_routes: &mut HashMap<RouteChannel, ManagementIdentity>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut RootChannels,
    session_identity: &mut SessionIdentities,
    push_buffer: &mut PushBuffer,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    metrics: &Arc<DispatchPathMetrics>,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
    delivery: &Arc<RouteDeliverySessions>,
) -> Result<(), SubcError> {
    decrement_counted_channel(&metrics.control_completion_queued);
    let (completion, ack_written) = match completion {
        ControlCompletion::ConfigureReady(completion) => (completion, false),
        ControlCompletion::AckWritten(completion) => (completion, true),
        ControlCompletion::AckCancelled(completion) => match *completion {
            ControlCompletion::AckWritten(completion) => (completion, false),
            ControlCompletion::ManagementAckWritten { route, .. } => {
                a0.retire(route);
                remove_installed_route(installed_route_epochs, route);
                return Ok(());
            }
            ControlCompletion::BindRetracted { route, .. } => {
                a0.retire(route);
                remove_installed_route(installed_route_epochs, route);
                return Ok(());
            }
            _ => return Err(SubcError::WriterClosed),
        },
        ControlCompletion::ManagementAckWritten {
            route, identity, ..
        } => {
            a0.retire(route);
            if installed_route_epochs.get(&route.channel) == Some(&route.epoch) {
                on_management_install(&identity);
                management_routes.insert(route, identity);
            }
            return Ok(());
        }
        ControlCompletion::WriteFailed => return Err(SubcError::WriterClosed),
        ControlCompletion::BindRetracted { route, .. } => {
            a0.retire(route);
            remove_installed_route(installed_route_epochs, route);
            return Ok(());
        }
    };
    handle_route_bind_completion(
        writer_tx,
        completion,
        ack_written,
        a0,
        routes,
        root_channels,
        session_identity,
        push_buffer,
        live_roots,
        pending_binds,
        installed_route_epochs,
        executor,
        shutdown,
        metrics,
        lifecycle_probe,
        delivery,
    )
    .await
}
