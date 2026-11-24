//! Close root lifecycle admission and retire work after route teardown.

use super::*;

#[cfg(test)]
mod tests;

// Runners reopen short-lived routes between steps. Keep the root warm across
// those gaps instead of cancelling the work that the next step will need.
pub(super) const GOODBYE_QUIESCE_GRACE: Duration = Duration::from_secs(60);

pub(super) fn has_live_route_bind(
    root: &ProjectRootId,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
) -> bool {
    pending_binds
        .values()
        .any(|bind| &bind.bind_root_id == root && !bind.cancelled && !bind.deadline_reported)
}

/// Deadlines live in the same loop-owned records as route admission. The loop
/// consumes both bind receipts and expired deadlines; no timer task can fence
/// a root after the loop has published its replacement route.
pub(super) fn settle_goodbye_quiesces(
    now: tokio::time::Instant,
    draining: bool,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    root_channels: &mut RootChannels,
    executor: &Arc<Executor>,
) {
    for (root, meta) in live_roots {
        if !meta
            .goodbye_quiesce_at
            .is_some_and(|due| draining || now >= due)
        {
            continue;
        }
        if root_channels.contains_key(root) {
            meta.goodbye_quiesce_at = None;
        } else if has_live_route_bind(root, pending_binds) {
            // A valid bind may have opened lifecycle admission on its way to
            // publishing the ACK. Let the receipt or failure settle the root.
            if draining {
                meta.goodbye_quiesce_at = None;
            }
        } else {
            root_channels.remove(root);
            quiesce_root(
                root,
                meta,
                executor,
                QuiescedLsp::ReapInBackground,
                if draining {
                    "module_drain"
                } else {
                    "goodbye_unbound"
                },
            );
        }
    }
}

pub(super) fn quiesce_after_bind_failure(
    root: &ProjectRootId,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    root_channels: &mut RootChannels,
    executor: &Arc<Executor>,
) {
    if let Some(meta) = live_roots.get_mut(root) {
        if let Some(due) = meta.goodbye_quiesce_at {
            if tokio::time::Instant::now() >= due
                && !root_channels.contains_key(root)
                && !has_live_route_bind(root, pending_binds)
            {
                root_channels.remove(root);
                quiesce_root(
                    root,
                    meta,
                    executor,
                    QuiescedLsp::ReapInBackground,
                    "goodbye_unbound",
                );
            }
            return;
        }
    }
    if root_channels.release_if_unbound(root, pending_binds, None) {
        quiesce_unbound_root(root, live_roots, executor);
    }
}

/// Who stops a root's running language servers once the root is quiesced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum QuiescedLsp {
    /// Detach the clients and shut them down on a background thread. Right for
    /// a route that closes while the module keeps running: nothing else would
    /// stop those servers.
    ReapInBackground,
    /// Refuse new starts but leave the clients in the manager, because the
    /// caller's exit phase shuts down every manager's servers together under
    /// one shared budget right after the module loop returns.
    LeaveForExitSweep,
}

pub(super) fn quiesce_unbound_root(
    root_id: &ProjectRootId,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
) {
    if let Some(meta) = live_roots.get_mut(root_id) {
        quiesce_root(
            root_id,
            meta,
            executor,
            QuiescedLsp::ReapInBackground,
            "immediate_unbound",
        );
    }
}

pub(super) fn quiesce_root(
    root_id: &ProjectRootId,
    meta: &mut RootMeta,
    executor: &Arc<Executor>,
    lsp: QuiescedLsp,
    cause: &str,
) {
    let had_grace = meta.goodbye_quiesce_at.take().is_some();
    let ctx = executor.actor_context(root_id);
    if let Some(ctx) = ctx.as_ref() {
        // Close lifecycle admission before touching scheduler queues. A running
        // ConfigureTail cannot release gates, install a watcher, or reserve a
        // callgraph build after this transition becomes visible.
        ctx.mark_subc_unbound();
        if had_grace {
            ctx.set_worker_only_routes(false);
        }
        // Parent folders have no local build progress to retain. Their watcher
        // owns exclusion retries independently of the child-reader session, so
        // release both lifetimes when the final route closes.
        if crate::parent_search::release_context(ctx) || ctx.root_too_broad() {
            ctx.stop_watcher_runtime();
            ctx.invalidate_artifacts_after_watcher_gap();
        }
        match lsp {
            QuiescedLsp::ReapInBackground => {
                // Detach under the root's manager lock before allowing a
                // rebind. The reaper owns only the old clients, never servers
                // a new route starts.
                let clients = ctx.lsp().quiesce();
                crate::lsp::manager::LspManager::spawn_idle_lsp_reap(clients);
            }
            QuiescedLsp::LeaveForExitSweep => ctx.lsp().close_start_admission(),
        }
        ctx.bash_background().clear_live_delivery_sessions();
    }
    // A bind replayed by the identity retry is this root's bind in flight.
    // Cancel it as route teardown cancels a route's own pending bind: queued,
    // it is removed; running, it stops at its next checkpoint before the
    // commit seal and publishes nothing. One that already sealed finishes, and
    // its completion quiesces the root again (`finish_identity_replay`).
    if let Some(replay) = meta.identity_replay.as_ref() {
        let outcome = executor.cancel_job(root_id, replay);
        log::debug!(
            "subc attach: cancelled identity retry bind for quiescing root {} ({outcome:?})",
            root_id.as_path().display()
        );
    }
    let cancelled = if root_id.as_path().is_dir() {
        executor.cancel_queued_root_maintenance(root_id)
    } else {
        executor.cancel_root_maintenance(root_id)
    };
    // Ordinary projects keep their watcher and resident artifacts across brief
    // unbinds, so a reconnect can replay observed changes without re-reading the
    // entire checkout. Parent folders and broad roots were stopped above.
    // Queued maintenance is dropped immediately, but running project builds and
    // inspect jobs retain a 120-second abandonment grace: their cancellation
    // checks stop them after that grace, or as soon as the checkout disappears.
    // The idle-TTL reaper stops project watchers later and marks their cached
    // artifacts for verification because changes after shutdown were not seen.
    let discarded = ctx
        .map(|ctx| crate::commands::configure::cancel_deferred_configure_maintenance(&ctx))
        .unwrap_or(0);
    meta.unbound_quiesced = true;
    meta.maintenance_queued_kinds.clear();
    meta.maintenance_pending = meta.maintenance_jobs_in_flight > 0;
    log::info!(
        "subc attach: quiesced unbound root {} (cancelled {} queued or running maintenance job(s), cancelled {} configure maintenance job(s)); cause={cause}",
        root_id.as_path().display(),
        cancelled,
        discarded
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn quiesce_connection_roots(
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut RootChannels,
    installed_route_epochs: &mut HashMap<u16, u32>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    active_tool_calls: &ActiveToolCalls,
    executor: &Arc<Executor>,
    delivery: &RouteDeliverySessions,
    lsp: QuiescedLsp,
) {
    cancel_all_active_tool_calls(active_tool_calls, executor, "connection teardown");
    for cancel in route_bash_cancels.values() {
        cancel.token.cancel();
    }
    route_bash_cancels.clear();

    let mut roots = live_roots.keys().cloned().collect::<HashSet<_>>();
    for pending in pending_binds.values_mut() {
        pending.cancelled = true;
        roots.insert(pending.bind_root_id.clone());
        let _ = executor.cancel_job(&pending.bind_root_id, &pending.cancellation);
    }

    // A connection exit abandons every installed route at once. Close lifecycle
    // admission before cancelling maintenance so no deferred worker can restore
    // root activity after the loop-owned route tables disappear.
    for root_id in roots {
        if let Some(meta) = live_roots.get_mut(&root_id) {
            quiesce_root(&root_id, meta, executor, lsp, "connection_end");
        } else if let Some(ctx) = executor.actor_context(&root_id) {
            ctx.mark_subc_unbound();
            executor.cancel_queued_root_maintenance(&root_id);
            crate::commands::configure::cancel_deferred_configure_maintenance(&ctx);
        }
    }

    for route in routes.values() {
        installed_routes::status_unbound(route);
        runner_notices::route_unbound(route);
    }
    routes.clear();
    route_delivery::publish_sessions(delivery, routes);
    root_channels.clear();
    installed_route_epochs.clear();
}
