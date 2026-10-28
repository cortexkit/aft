//! Call-scoped Basal routes delegate read-only operations, never general tools.
use super::*;
use crate::commands::file_read_exact::{self, ReadExactRefusal as Refusal};
use subc_protocol::{
    scope::{ScopeKind, ScopeStamp, ScopeStatus},
    session::ModuleControlResponseToModule,
};

pub(super) fn principal_is_flow_read_carrier(principal: &AuthenticatedPrincipal) -> bool {
    matches!(principal, AuthenticatedPrincipal::RouteBind { principal_id: Some(id), .. } if id == "reserved:basal")
}

fn route_checks(identity: &RouteIdentity) -> Result<&ScopeStamp, Refusal> {
    if !principal_is_flow_read_carrier(&identity.spawn_principal) {
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
        return Err(Refusal::ParentNotAdmitted(
            "the flow-read scope carries no parent scope".into(),
        ));
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
    let ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch,
        scope,
        ..
    } = reply
    else {
        return Err(Refusal::ParentNotAdmitted(
            "the daemon did not return a scope.describe response for the parent scope".into(),
        ));
    };
    let reason = match status {
        ScopeStatus::NotLive => "the daemon reports the parent scope is not live".into(),
        ScopeStatus::Ended => "the daemon reports the parent scope ended".into(),
        ScopeStatus::Live => {
            let epoch = scope_epoch.ok_or_else(|| {
                Refusal::ParentNotAdmitted(format!(
                    "the daemon reports the parent scope is live without an epoch (expected {})",
                    expected.scope_epoch
                ))
            })?;
            if epoch != expected.scope_epoch {
                return Err(Refusal::ParentNotAdmitted(format!(
                    "the daemon reports the parent scope is live at epoch {epoch}, not {}",
                    expected.scope_epoch
                )));
            }
            let stamp = scope.as_ref().ok_or_else(|| {
                Refusal::ParentNotAdmitted(
                    "the daemon reports the parent scope is live without a scope stamp".into(),
                )
            })?;
            if stamp.scope_epoch != epoch {
                format!(
                    "the daemon's parent scope stamp has epoch {}, not {epoch}",
                    stamp.scope_epoch
                )
            } else if stamp.owner != expected.owner {
                "the daemon's parent scope owner does not match the requested parent".into()
            } else if stamp.scope_ref != expected.scope_ref {
                "the daemon's parent scope reference does not match the requested parent".into()
            } else if stamp.kind != ScopeKind::Head {
                "the daemon's parent scope is not a head scope".into()
            } else if !stamp.owner_authorized {
                "the daemon's parent scope is not owner-authorized".into()
            } else if stamp
                .expires_at_ms
                .is_some_and(|at| at <= crate::db::call_ledger::now_ms() as u64)
            {
                "the daemon's parent scope has expired".into()
            } else {
                return Ok(());
            }
        }
    };
    Err(Refusal::ParentNotAdmitted(reason))
}

fn parent_route<'a>(
    stamp: &ScopeStamp,
    routes: &'a HashMap<RouteChannel, RouteIdentity>,
) -> Result<&'a RouteIdentity, Refusal> {
    let parent = stamp.parent.as_ref().ok_or_else(|| {
        Refusal::ParentNotAdmitted("the flow-read scope carries no parent scope".into())
    })?;
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
        .ok_or_else(|| {
            let mut reason = format!(
                "no open AFT route for the parent head scope {} at epoch {} (the head must have an AFT route open at the epoch core stamped)",
                parent.scope_ref, parent.scope_epoch
            );
            let epochs = routes.values()
                .filter_map(|route| route.scope.as_ref())
                .filter(|scope| scope.owner == parent.owner && scope.scope_ref == parent.scope_ref && scope.scope_epoch != parent.scope_epoch)
                .map(|scope| scope.scope_epoch)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            if !epochs.is_empty() {
                reason.push_str(&format!("; open routes for that owner and scope reference have epochs {epochs:?}"));
            }
            Refusal::ParentNotAdmitted(reason)
        })
}

/// Intercept before watcher restoration, catalogs, grant operations or tool dispatch.
pub(super) fn intercept(identity: &RouteIdentity, envelope: &Value) -> bool {
    identity
        .scope
        .as_ref()
        .is_some_and(|s| s.scope_ref.starts_with("flow-read/"))
        || envelope.get("op").and_then(Value::as_str) == Some(file_read_exact::OPERATION)
        || envelope.get("name").and_then(Value::as_str) == Some(file_read_exact::OPERATION)
        || envelope.get("op").and_then(Value::as_str) == Some(super::perf_sweep::OPERATION)
        || envelope.get("name").and_then(Value::as_str) == Some(super::perf_sweep::OPERATION)
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
    let wait = if envelope.get("op").and_then(Value::as_str) == Some(super::perf_sweep::OPERATION) {
        super::perf_sweep::DISPATCH_BUDGET
    } else {
        persistence::WAIT_BUDGET
    };
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
        wait,
        #[cfg(test)]
        DispatchHooks::default(),
    )
    .await
}

#[cfg(test)]
#[derive(Default)]
struct DispatchHooks {
    submitted: Option<oneshot::Sender<()>>,
    before_read: Option<Box<dyn FnOnce() + Send>>,
}

/// `parent_wait` bounds the entire dispatch, including daemon admission,
/// parent-policy readiness, and the executor's queue and run. Tests shorten it.
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
    #[cfg(test)] hooks: DispatchHooks,
) -> Result<(), SubcError> {
    let limit = Instant::now() + parent_wait;
    let operation = match envelope.get("op").and_then(Value::as_str) {
        Some(super::perf_sweep::OPERATION) => super::perf_sweep::OPERATION,
        _ => file_read_exact::OPERATION,
    };
    let admit = || {
        if envelope.get("op").and_then(Value::as_str) != Some(operation) {
            return Err(Refusal::RouteNotAdmitted("operation"));
        }
        let stamp = route_checks(&identity)?;
        let parent = parent_route(stamp, routes)?.clone();
        let control = control.cloned().ok_or_else(|| {
            Refusal::ParentNotAdmitted(
                "cannot ask the daemon whether the parent scope is live".into(),
            )
        })?;
        let expiry = read_deadline(stamp)?;
        Ok((parent, control, expiry.min(limit), expiry <= limit))
    };
    let (parent, control, deadline, expires_first) = match admit() {
        Ok(admitted) => admitted,
        Err(error) => return send_provider_error(tx, metrics, frame, error.error()).await,
    };
    let sweep = if operation == super::perf_sweep::OPERATION {
        match super::perf_sweep::admit(&envelope) {
            Ok(guard) => Some(guard),
            Err(error) => return send_provider_error(tx, metrics, frame, error).await,
        }
    } else {
        None
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
                tool: operation.into(),
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
        let child = identity
            .scope
            .as_ref()
            .expect("child route was checked")
            .clone();
        let dispatch = async {
            // The frame loop must remain free to deliver this control RPC's answer.
            let parent_stamp = parent.scope.as_ref().expect("parent route was checked");
            let admission = match control.describe(parent_stamp).await {
                Ok(reply) => live_parent(parent_stamp, &reply)
                    .and_then(|_| unexpired(identity.scope.as_ref().unwrap())),
                Err(_) => Err(Refusal::ParentNotAdmitted(
                    "the daemon scope.describe request for the parent scope failed".into(),
                )),
            };
            match admission {
                Err(error) => Err(error.error()),
                Ok(()) => {
                    if let Some(ctx) = executor.actor_context(&parent.root) {
                        wait_for_parent_policy(&ctx, access_grants::scope_key(&parent), deadline)
                            .await;
                    }
                    if !active_tool_call_is_registered(&active, route, corr) {
                        return Err(Refusal::IoError.error());
                    }
                    if let Some(sweep) = sweep {
                        // perf.sweep samples the daemon (a profiler child and
                        // the daemon log), not the parent project. As an
                        // executor job it would hold the project's read lane
                        // for the whole sample, up to 35 s, and stall every
                        // write on that project. It runs on its own thread,
                        // still bounded by `deadline`. The busy guard moves
                        // with it, so a second sweep stays refused until this
                        // sample ends, even after the deadline has answered.
                        let child = child.clone();
                        let (done, answer) = oneshot::channel();
                        #[cfg(test)]
                        let before_read = hooks.before_read;
                        let spawned = std::thread::Builder::new()
                            .name("aft-perf-sweep".into())
                            .spawn(move || {
                                let _sweep = sweep;
                                #[cfg(test)]
                                if let Some(before_read) = before_read {
                                    before_read();
                                }
                                let _ = done.send(
                                    unexpired(&child)
                                        .map_err(Refusal::error)
                                        .and_then(|_| super::perf_sweep::run(&envelope, deadline)),
                                );
                            });
                        if spawned.is_err() {
                            return Err(Refusal::IoError.error());
                        }
                        #[cfg(test)]
                        if let Some(submitted) = hooks.submitted {
                            let _ = submitted.send(());
                        }
                        // A sweep thread that panics drops `done` unanswered.
                        return answer
                            .await
                            .unwrap_or_else(|_| Err(Refusal::IoError.error()));
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
                        operation,
                        Box::new(move |ctx| {
                            #[cfg(test)]
                            if let Some(before_read) = hooks.before_read {
                                before_read();
                            }
                            let result =
                                read_as_parent(&identity, &parent, &envelope, ctx, deadline)
                                    .map_err(Refusal::error);
                            Response::success(
                                job_id,
                                match result {
                                    Ok(value) => json!({"answer": value}),
                                    Err(error) => json!({"error": error}),
                                },
                            )
                        }),
                    );
                    #[cfg(test)]
                    if let Some(submitted) = hooks.submitted {
                        let _ = submitted.send(());
                    }
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
            }
        };
        let result =
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), dispatch).await
            {
                Ok(result) if Instant::now() < deadline => result,
                _ => {
                    let job = active
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&(route, corr))
                        .map(|call| (call.root_id.clone(), call.cancellation.clone()));
                    if let Some((root, cancellation)) = job {
                        // Remove queued work before it can start. Running reads retain
                        // their inputs but their response receiver has been dropped.
                        executor.cancel_job_before_execution(&root, &cancellation);
                    }
                    let error = if expires_first || unexpired(&child).is_err() {
                        Refusal::RouteNotAdmitted("expiry")
                    } else {
                        Refusal::PolicyTimeout
                    };
                    Err(error.error())
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
            let _ = send_reliable_writer_frame(&tx, &metrics, frame, "flow operation reply").await;
        }
        finish_active_tool_call(&active, route, corr);
    });
    Ok(())
}

/// `limit` is the read's own time limit. Waiting for the database lock to
/// read the parent's access policy stops at it (or at the scope's expiry, if
/// sooner).
fn read_as_parent(
    identity: &RouteIdentity,
    parent: &RouteIdentity,
    envelope: &Value,
    ctx: &AppContext,
    limit: Instant,
) -> Result<Value, Refusal> {
    read_as_parent_with_hooks(
        identity,
        parent,
        envelope,
        ctx,
        limit,
        file_read_exact::ReadHooks::default(),
    )
}

fn read_as_parent_with_hooks(
    identity: &RouteIdentity,
    parent: &RouteIdentity,
    envelope: &Value,
    ctx: &AppContext,
    limit: Instant,
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
    let policy_deadline = deadline.min(limit);
    let snapshot = || {
        parent_policy_snapshot(ctx, &key, policy_deadline).map_err(|error| {
            // A wait cut short by the scope's expiry reports the expiry, as
            // every other expiry check here does.
            if error == Refusal::PolicyTimeout && unexpired(stamp).is_err() {
                Refusal::RouteNotAdmitted("expiry")
            } else {
                error
            }
        })
    };
    let initial = snapshot()?;
    let grant_denial = access_grants::scope_grants_denial_for_parent(identity, Some(parent));
    let storage = ctx.storage_dir();
    let read_storage = storage.clone();
    let root = parent.project_root.clone();
    let read = timed_io(stamp, deadline, &parent.project_root, move |_| {
        let _grants = crate::access_policy::scope_grants_guard(grant_denial);
        let policy = initial.resolve(&read_storage);
        file_read_exact::read_exact(&path, &root, &policy, hooks)
    })?;

    // Recheck the actual resolved target, not an alias that may now point elsewhere.
    // Each snapshot releases its database lock before its filesystem work starts.
    let current = snapshot()?;
    let allowed = timed_io(stamp, deadline, &read.resolved, move |resolved| {
        let _grants = crate::access_policy::scope_grants_guard(grant_denial);
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

/// Copy the parent's stored access policy, waiting for the aft.db lock no
/// later than `deadline`; a lock still held then is `PolicyTimeout`.
fn parent_policy_snapshot(
    ctx: &AppContext,
    key: &crate::db::access_grants::ScopeKey,
    deadline: Instant,
) -> Result<access_grants::PolicySnapshot, Refusal> {
    let db = ctx.db().ok_or(Refusal::IoError)?;
    let conn = lock_by(&db, deadline)?;
    access_grants::policy_snapshot(&conn, key)
        .map_err(|_| Refusal::IoError)?
        .ok_or(Refusal::AccessDenied)
}

/// Longest pause between attempts to take a busy aft.db lock.
const LOCK_RETRY_MAX: Duration = Duration::from_millis(20);

/// Take the aft.db lock, waiting no later than `deadline`. Other routes hold
/// this lock for unbounded work, so a plain blocking `lock()` could keep the
/// read past its time limit. Polling with `try_lock` leaves no thread behind
/// still waiting for the lock once the read has given up. The lock is always
/// tried once, so a free lock is taken even when the deadline has passed.
fn lock_by(
    db: &StdMutex<crate::db::TrackedConnection>,
    deadline: Instant,
) -> Result<std::sync::MutexGuard<'_, crate::db::TrackedConnection>, Refusal> {
    let mut pause = Duration::from_millis(1);
    loop {
        match db.try_lock() {
            Ok(conn) => return Ok(conn),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(Refusal::IoError),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Refusal::PolicyTimeout);
        }
        std::thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(LOCK_RETRY_MAX);
    }
}

/// How often a waiting read looks for its parent's scope row even without a
/// wake-up, in case the row was written by a path that sends none.
const SCOPE_RECHECK: Duration = Duration::from_millis(100);

/// The parent head's bind is acknowledged before its database opens and its
/// scope row is registered, and both happen in queued jobs that this read's
/// pure-read lane can overtake. Wait, up to `deadline`, until the database is
/// open and the parent's scope row exists. The dispatch deadline ends the
/// request if readiness never arrives; a failed or unconfigured database
/// ends this wait at once so the read can report that failure.
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
        // The dispatch deadline also bounds a probe waiting to start on the
        // blocking pool. An abandoned probe only returns to a dropped handle;
        // this async task alone owns the terminal reply.
        let probe = tokio::task::spawn_blocking(move || {
            parent_policy_snapshot(&probe_ctx, &probe_key, deadline)
        })
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
