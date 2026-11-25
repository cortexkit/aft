//! Installed-route bookkeeping, including per-runner status key-value lifetimes.
use super::*;
use crate::subc_config::BindConfigCache;

#[derive(Default)]
pub(super) struct RootChannels {
    pub(super) channels: HashMap<ProjectRootId, HashSet<RouteChannel>>,
    pub(super) config: BindConfigCache,
    projects: HashMap<ProjectRootId, Arc<settings::Project>>,
}

impl std::ops::Deref for RootChannels {
    type Target = HashMap<ProjectRootId, HashSet<RouteChannel>>;

    fn deref(&self) -> &Self::Target {
        &self.channels
    }
}

impl RootChannels {
    pub(super) fn select_project(
        &self,
        root: &ProjectRootId,
        incoming: Option<Arc<settings::Project>>,
    ) -> Result<Option<Arc<settings::Project>>, &'static str> {
        let current = self.projects.get(root);
        match (current, incoming.as_ref()) {
            (Some(current), Some(next))
                if current.registered() && next.registered() && current.id != next.id =>
            {
                log::warn!(
                    "project settings conflict for {}: bound to {}, requested {}",
                    root.as_path().display(),
                    current.id,
                    next.id
                );
                Err("configure failed during route bind")
            }
            (_, Some(next)) if next.registered() => Ok(incoming),
            (Some(current), _) => Ok(Some(current.clone())),
            _ => Ok(incoming),
        }
    }

    pub(super) fn remember_project(
        &mut self,
        root: &ProjectRootId,
        project: Arc<settings::Project>,
    ) {
        if self.contains_key(root)
            && project.registered()
            && !self
                .projects
                .get(root)
                .is_some_and(|current| current.registered())
        {
            log::info!(
                "project settings upgraded for {}: project {}",
                root.as_path().display(),
                project.id
            );
        }
        self.projects.insert(root.clone(), project);
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn remove(&mut self, root: &ProjectRootId) {
        self.channels.remove(root);
        self.config.release_project(root.as_path());
        self.projects.remove(root);
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
            self.remove(root);
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
    use crate::subc::test_support::{route_identity, test_root};

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
