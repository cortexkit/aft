//! Installed-route bookkeeping, including per-runner status key-value lifetimes.
use super::*;

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
    fn status_kv_tracks_installed_routes_and_channel_replacement() {
        use crate::late_result_bus::status;
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
            status::test_value(session).unwrap()["segment"],
            "[AFT E0 W0 | T1]"
        );
        remove_route_channel(&mut routes, &mut roots, first);
        assert!(status::test_value(session).is_some());
        insert_route_channel(
            &mut routes,
            &mut roots,
            second,
            route_identity(&root, other),
        );
        assert!(
            status::test_value(session).is_none(),
            "replacement ends the last old-session route"
        );
        assert!(
            status::test_value(other).is_none(),
            "a bound route alone does not publish a segment"
        );
        client.publish(root.as_path(), "opencode", session, "late completion");
        assert!(status::test_value(session).is_none());
        client.publish(root.as_path(), "opencode", other, "[AFT E1 W0 | T2]");
        assert!(status::test_value(other).is_some());
        remove_route_channel(&mut routes, &mut roots, second);
        assert!(status::test_value(other).is_none());
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
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
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
