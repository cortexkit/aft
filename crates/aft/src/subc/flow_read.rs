//! A call-scoped module route delegates only exact reads, never general tools.
use super::*;
use crate::commands::file_read_exact::{self, ReadExactRefusal as Refusal};
use subc_protocol::{
    scope::{ScopeKind, ScopeStamp, ScopeStatus},
    session::ModuleControlResponseToModule,
};

fn route_checks(identity: &RouteIdentity) -> Result<&ScopeStamp, Refusal> {
    if !matches!(&identity.spawn_principal, AuthenticatedPrincipal::RouteBind { principal_id: Some(id), .. } if id == "reserved:basal")
    {
        return Err(Refusal::RouteNotAdmitted("principal"));
    }
    let stamp = identity
        .scope
        .as_ref()
        .ok_or(Refusal::RouteNotAdmitted("owner"))?;
    if !stamp.owner_authorized
        || !matches!(&stamp.owner, Principal::Reserved { module_id } if module_id == "prefrontal-core")
    {
        return Err(Refusal::RouteNotAdmitted("owner"));
    }
    if stamp.kind != ScopeKind::Ephemeral {
        return Err(Refusal::RouteNotAdmitted("kind"));
    }
    if !stamp.scope_ref.starts_with("flow-read/") {
        return Err(Refusal::RouteNotAdmitted("scope_ref"));
    }
    if stamp.parent.is_none() {
        return Err(Refusal::RouteNotAdmitted("parent"));
    }
    unexpired(stamp)?;
    Ok(stamp)
}

fn unexpired(stamp: &ScopeStamp) -> Result<(), Refusal> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if stamp.expires_at_ms.is_none_or(|at| u128::from(at) <= now) {
        return Err(Refusal::RouteNotAdmitted("expiry"));
    }
    Ok(())
}

fn live_parent(
    expected: &ScopeStamp,
    reply: &ModuleControlResponseToModule,
) -> Result<(), Refusal> {
    match reply {
        ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            scope_epoch: Some(epoch),
            scope: Some(stamp),
            ..
        } if *epoch == expected.scope_epoch
            && stamp.scope_epoch == *epoch
            && stamp.owner == expected.owner
            && stamp.scope_ref == expected.scope_ref
            && stamp.kind == ScopeKind::Head
            && stamp.owner_authorized
            && stamp
                .expires_at_ms
                .is_none_or(|at| at > crate::db::call_ledger::now_ms() as u64) =>
        {
            Ok(())
        }
        _ => Err(Refusal::RouteNotAdmitted("parent")),
    }
}

fn parent_route<'a>(
    stamp: &ScopeStamp,
    routes: &'a HashMap<RouteChannel, RouteIdentity>,
) -> Result<&'a RouteIdentity, Refusal> {
    let parent = stamp
        .parent
        .as_ref()
        .ok_or(Refusal::RouteNotAdmitted("parent"))?;
    routes
        .values()
        .find(|route| {
            route.scope.as_ref().is_some_and(|s| {
                s.owner == parent.owner
                    && s.scope_ref == parent.scope_ref
                    && s.scope_epoch == parent.scope_epoch
                    && s.kind == ScopeKind::Head
                    && s.owner_authorized
            })
        })
        .ok_or(Refusal::RouteNotAdmitted("parent"))
}

/// Intercept before watcher restoration, catalogs, grant operations or tool dispatch.
pub(super) fn intercept(identity: &RouteIdentity, envelope: &Value) -> bool {
    identity
        .scope
        .as_ref()
        .is_some_and(|s| s.scope_ref.starts_with("flow-read/"))
        || envelope.get("op").and_then(Value::as_str) == Some(file_read_exact::OPERATION)
        || envelope.get("name").and_then(Value::as_str) == Some(file_read_exact::OPERATION)
}

pub(super) fn admit_management(scope: Option<&ScopeStamp>) -> Result<(), ErrorBody> {
    if scope.is_some_and(|s| s.scope_ref.starts_with("flow-read/")) {
        return Err(Refusal::RouteNotAdmitted("operation").error());
    }
    Ok(())
}

pub(super) async fn dispatch(
    tx: &WriterSender,
    metrics: &Arc<DispatchPathMetrics>,
    frame: &Frame,
    identity: RouteIdentity,
    envelope: Value,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    executor: &Arc<Executor>,
    active: &ActiveToolCalls,
    control: Option<&Arc<readiness::DaemonControl>>,
) -> Result<(), SubcError> {
    dispatch_with_wait(
        tx,
        metrics,
        frame,
        identity,
        envelope,
        routes,
        executor,
        active,
        control,
        persistence::WAIT_BUDGET,
    )
    .await
}

/// `parent_wait` bounds how long the read waits for the parent head's
/// database and scope registration; tests shorten it.
#[allow(clippy::too_many_arguments)]
async fn dispatch_with_wait(
    tx: &WriterSender,
    metrics: &Arc<DispatchPathMetrics>,
    frame: &Frame,
    identity: RouteIdentity,
    envelope: Value,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    executor: &Arc<Executor>,
    active: &ActiveToolCalls,
    control: Option<&Arc<readiness::DaemonControl>>,
    parent_wait: Duration,
) -> Result<(), SubcError> {
    let admit = || {
        if envelope.get("op").and_then(Value::as_str) != Some(file_read_exact::OPERATION) {
            return Err(Refusal::RouteNotAdmitted("operation"));
        }
        let stamp = route_checks(&identity)?;
        let parent = parent_route(stamp, routes)?.clone();
        let control = control
            .cloned()
            .ok_or(Refusal::RouteNotAdmitted("parent"))?;
        Ok((parent, control))
    };
    let (parent, control) = match admit() {
        Ok(admitted) => admitted,
        Err(error) => return send_provider_error(tx, metrics, frame, error.error()).await,
    };
    let route = route_key(frame.header.channel, frame.header.epoch);
    let corr = frame.header.corr;
    let request = RequestFrameMeta {
        ver: frame.header.ver,
        flags: frame.header.flags,
    };
    active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            (route, corr),
            ActiveToolCall {
                root_id: parent.root.clone(),
                cancellation: JobCancellation::new(),
                detach_policy: RouteDetachPolicy::CancelOnDetach,
                tool: file_read_exact::OPERATION.into(),
                started_at: Instant::now(),
                request,
                answering: false,
            },
        );
    let (tx, metrics, executor, active) = (
        tx.clone(),
        metrics.clone(),
        executor.clone(),
        active.clone(),
    );
    tokio::spawn(async move {
        // The frame loop must remain free to deliver this control RPC's answer.
        let parent_stamp = parent.scope.as_ref().expect("parent route was checked");
        let admission = match control.describe(parent_stamp).await {
            Ok(reply) => live_parent(parent_stamp, &reply)
                .and_then(|_| unexpired(identity.scope.as_ref().unwrap())),
            Err(_) => Err(Refusal::RouteNotAdmitted("parent")),
        };
        let result = match admission {
            Err(error) => Err(error.error()),
            Ok(()) => {
                if let Some(ctx) = executor.actor_context(&parent.root) {
                    let child = identity.scope.as_ref().expect("child route was checked");
                    let mut deadline = Instant::now() + parent_wait;
                    if let Ok(expiry) = read_deadline(child) {
                        deadline = deadline.min(expiry);
                    }
                    wait_for_parent_policy(&ctx, access_grants::scope_key(&parent), deadline).await;
                }
                if !active_tool_call_is_registered(&active, route, corr) {
                    return;
                }
                let request_id = format!("flow-read-{}-{corr}", route.channel);
                let job_id = request_id.clone();
                let rx = resubmit_active_tool_call(
                    &executor,
                    &active,
                    route,
                    corr,
                    parent.root.clone(),
                    Lane::PureRead,
                    request_id.clone(),
                    file_read_exact::OPERATION,
                    Box::new(move |ctx| {
                        let result = read_as_parent(&identity, &parent, &envelope, ctx);
                        Response::success(
                            job_id,
                            match result {
                                Ok(value) => json!({"answer": value}),
                                Err(error) => json!({"error": error.error()}),
                            },
                        )
                    }),
                );
                let response = await_executor_response(rx, request_id).await;
                if let Some(value) = response.data.get("answer") {
                    Ok(value.clone())
                } else if let Some(error) = response.data.get("error") {
                    Err(serde_json::from_value::<ErrorBody>(error.clone())
                        .unwrap_or_else(|_| Refusal::IoError.error()))
                } else {
                    Err(Refusal::IoError.error())
                }
            }
        };
        if !claim_active_tool_call(&active, route, corr) {
            return;
        }
        let (ty, body) = match result {
            Ok(value) => (FrameType::Response, value),
            Err(error) => (FrameType::Error, serde_json::to_value(error).unwrap()),
        };
        if let Ok(frame) = Frame::build_with_version(
            request.ver,
            ty,
            request.flags,
            route.channel,
            route.epoch,
            corr,
            serde_json::to_vec(&body).unwrap(),
        ) {
            let _ = send_reliable_writer_frame(&tx, &metrics, frame, "exact read reply").await;
        }
        finish_active_tool_call(&active, route, corr);
    });
    Ok(())
}

fn read_as_parent(
    identity: &RouteIdentity,
    parent: &RouteIdentity,
    envelope: &Value,
    ctx: &AppContext,
) -> Result<Value, Refusal> {
    read_as_parent_with_hooks(
        identity,
        parent,
        envelope,
        ctx,
        file_read_exact::ReadHooks::default(),
    )
}

fn read_as_parent_with_hooks(
    identity: &RouteIdentity,
    parent: &RouteIdentity,
    envelope: &Value,
    ctx: &AppContext,
    hooks: file_read_exact::ReadHooks,
) -> Result<Value, Refusal> {
    use crate::access_policy::{AccessDecision, AccessOperation};
    let stamp = identity
        .scope
        .as_ref()
        .ok_or(Refusal::RouteNotAdmitted("owner"))?;
    unexpired(stamp)?;
    let path = envelope
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty() && !path.contains('\0'))
        .ok_or(Refusal::BadRequest)?;
    let path = PathBuf::from(path);
    let deadline = read_deadline(stamp)?;
    let key = access_grants::scope_key(parent);
    let initial = parent_policy_snapshot(ctx, &key)?;
    let storage = ctx.storage_dir();
    let read_storage = storage.clone();
    let root = parent.project_root.clone();
    let read = timed_io(stamp, deadline, &parent.project_root, move |_| {
        let policy = initial.resolve(&read_storage);
        file_read_exact::read_exact(&path, &root, &policy, hooks)
    })?;

    // Recheck the actual resolved target, not an alias that may now point elsewhere.
    // Each snapshot releases its database lock before its filesystem work starts.
    let current = parent_policy_snapshot(ctx, &key)?;
    let allowed = timed_io(stamp, deadline, &read.resolved, move |resolved| {
        Ok(current
            .resolve(&storage)
            .decide(AccessOperation::Read, &resolved, false))
    })?;
    if allowed != AccessDecision::Allow {
        return Err(Refusal::AccessDenied);
    }
    before_deadline(stamp, deadline)?;
    Ok(read.value)
}

fn parent_policy_snapshot(
    ctx: &AppContext,
    key: &crate::db::access_grants::ScopeKey,
) -> Result<access_grants::PolicySnapshot, Refusal> {
    let db = ctx.db().ok_or(Refusal::IoError)?;
    let conn = db.lock().map_err(|_| Refusal::IoError)?;
    access_grants::policy_snapshot(&conn, key)
        .map_err(|_| Refusal::IoError)?
        .ok_or(Refusal::AccessDenied)
}

/// How often a waiting read looks for its parent's scope row even without a
/// wake-up, in case the row was written by a path that sends none.
const SCOPE_RECHECK: Duration = Duration::from_millis(100);

/// The parent head's bind is acknowledged before its database opens and its
/// scope row is registered, and both happen in queued jobs that this read's
/// pure-read lane can overtake. Wait, up to `deadline`, until the database is
/// open and the parent's scope row exists. The wait never decides the
/// outcome: the read itself still refuses if either is missing afterwards,
/// and a failed or unconfigured database ends the wait at once.
///
/// This runs on the read's own async task, not on an executor worker, and
/// each look at the row runs on a blocking thread so a held database lock
/// never stalls the async runtime.
async fn wait_for_parent_policy(
    ctx: &Arc<AppContext>,
    key: crate::db::access_grants::ScopeKey,
    deadline: Instant,
) {
    // A staged open must not wait for the bind's configure tail to start it.
    crate::database_open::dispatch(ctx);
    ctx.wait_for_database_settled(deadline).await;
    loop {
        let registered = access_grants::scope_registered().notified();
        tokio::pin!(registered);
        registered.as_mut().enable();
        if ctx.db().is_none() {
            return;
        }
        let (probe_ctx, probe_key) = (ctx.clone(), key.clone());
        let probe =
            tokio::task::spawn_blocking(move || parent_policy_snapshot(&probe_ctx, &probe_key))
                .await;
        // Only a missing row is worth waiting for; any other answer is final.
        if !matches!(probe, Ok(Err(Refusal::AccessDenied))) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let wake = tokio::time::Instant::from_std(deadline.min(now + SCOPE_RECHECK));
        let _ = tokio::time::timeout_at(wake, registered).await;
    }
}

fn read_deadline(stamp: &ScopeStamp) -> Result<Instant, Refusal> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let remaining = stamp
        .expires_at_ms
        .and_then(|at| u128::from(at).checked_sub(now))
        .filter(|remaining| *remaining > 0)
        .ok_or(Refusal::RouteNotAdmitted("expiry"))?;
    Instant::now()
        .checked_add(Duration::from_millis(remaining as u64))
        .ok_or(Refusal::RouteNotAdmitted("expiry"))
}

fn before_deadline(stamp: &ScopeStamp, deadline: Instant) -> Result<(), Refusal> {
    unexpired(stamp)?;
    if Instant::now() >= deadline {
        return Err(Refusal::RouteNotAdmitted("expiry"));
    }
    Ok(())
}

fn timed_io<T: Send + 'static>(
    stamp: &ScopeStamp,
    deadline: Instant,
    path: &Path,
    work: impl FnOnce(PathBuf) -> Result<T, Refusal> + Send + 'static,
) -> Result<T, Refusal> {
    before_deadline(stamp, deadline)?;
    // A stalled helper retains only owned read inputs and a bounded pool permit,
    // never the shared database connection or the executor's actor context.
    let result = crate::bounded_io::run(path, Some(deadline), move |path| Ok(work(path)));
    before_deadline(stamp, deadline)?;
    result.map_err(|_| Refusal::IoError)?
}

#[cfg(test)]
#[path = "flow_read_tests.rs"]
mod tests;
