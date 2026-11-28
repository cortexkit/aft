//! Refresh delivery and maintenance policy from the installed route table.

use super::*;

pub(super) fn publish_sessions(
    delivery: &RouteDeliverySessions,
    routes: &HashMap<RouteChannel, RouteIdentity>,
) {
    delivery.publish(
        routes
            .values()
            .filter(|route| route.trust.allows_bash_observation())
            .map(|route| route.session.as_str())
            .collect(),
    );
}

pub(super) fn prepare_bind(
    executor: &Executor,
    delivery: &Arc<RouteDeliverySessions>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    metrics: &DispatchPathMetrics,
) {
    // Configure replay needs the current fleet snapshot, even though this
    // route will not be installed until after its acknowledgment is written.
    let ctx = actor_context(executor, root, Some(metrics));
    sync_bound_root(
        ctx.as_deref(),
        delivery,
        routes,
        root_channels,
        root,
        metrics,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_route(
    executor: &Executor,
    delivery: &Arc<RouteDeliverySessions>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
    identity: RouteIdentity,
    additional_root: &ProjectRootId,
    metrics: &DispatchPathMetrics,
) {
    let root = identity.root.clone();
    let previous_root = routes.get(&channel).map(|route| route.root.clone());
    let ctx = actor_context(executor, &root, Some(metrics));
    installed_routes::insert_route_channel_with_actor(
        routes,
        root_channels,
        channel,
        identity,
        ctx.as_deref(),
    );
    publish_sessions(delivery, routes);
    sync_bound_root(
        ctx.as_deref(),
        delivery,
        routes,
        root_channels,
        &root,
        metrics,
    );
    if additional_root != &root {
        sync_policy(executor, routes, root_channels, additional_root, metrics);
    }
    if let Some(previous_root) = previous_root.filter(|old| old != &root && old != additional_root)
    {
        sync_policy(executor, routes, root_channels, &previous_root, metrics);
    }
}

fn sync_bound_root(
    ctx: Option<&AppContext>,
    delivery: &Arc<RouteDeliverySessions>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    metrics: &DispatchPathMetrics,
) {
    if let Some(ctx) = ctx {
        apply_policy(ctx, root, indexed_worker_only(routes, root_channels, root));
        metrics.mark_frame_loop_wait_for_root(
            "bash.live_delivery_sessions",
            root,
            ctx.bash_background().live_delivery_lock_diagnostics(),
        );
        ctx.bash_background().use_route_delivery_sessions(delivery);
    }
}

pub(super) fn sync_route_change(
    executor: &Executor,
    delivery: &RouteDeliverySessions,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    metrics: &DispatchPathMetrics,
) {
    publish_sessions(delivery, routes);
    sync_policy(executor, routes, root_channels, root, metrics);
}

fn sync_policy(
    executor: &Executor,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    metrics: &DispatchPathMetrics,
) {
    if let Some(ctx) = actor_context(executor, root, Some(metrics)) {
        apply_policy(&ctx, root, indexed_worker_only(routes, root_channels, root));
    }
}

fn indexed_worker_only(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
) -> bool {
    root_channels.get(root).is_some_and(|channels| {
        !channels.is_empty()
            && channels
                .iter()
                .all(|channel| routes.get(channel).is_some_and(is_worker))
    })
}

fn is_worker(route: &RouteIdentity) -> bool {
    route
        .scope
        .as_ref()
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Worker)
}

/// Reclamation can discard routes from several indexes. Reconcile policy
/// from the authoritative table, including the root that may just have lost
/// its last route.
pub(super) fn sync_all_callgraph_refresh_policies(
    executor: &Executor,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    additional_root: Option<&ProjectRootId>,
    metrics: Option<&DispatchPathMetrics>,
) {
    let mut worker_only_by_root = HashMap::new();
    for route in routes.values() {
        *worker_only_by_root.entry(&route.root).or_insert(true) &= is_worker(route);
    }
    if let Some(root) = additional_root {
        worker_only_by_root.entry(root).or_insert(false);
    }
    for (root, worker_only) in worker_only_by_root {
        if let Some(ctx) = actor_context(executor, root, metrics) {
            apply_policy(&ctx, root, worker_only);
        }
    }
}

fn actor_context(
    executor: &Executor,
    root: &ProjectRootId,
    metrics: Option<&DispatchPathMetrics>,
) -> Option<Arc<AppContext>> {
    if let Some(metrics) = metrics {
        metrics.mark_frame_loop_wait_for_root(
            "executor.scheduler/actor_context",
            root,
            executor.scheduler_lock_diagnostics(),
        );
    }
    executor.actor_context(root)
}

fn apply_policy(ctx: &AppContext, _root: &ProjectRootId, worker_only: bool) {
    #[cfg(test)]
    tests::record_policy_visit(_root);
    ctx.set_worker_only_routes(worker_only);
}

#[cfg(test)]
mod tests;
