//! Persistence readiness waits must not occupy the frame loop or an executor worker.

use super::*;

/// How long a call may wait for its root's persistence before it is admitted
/// anyway and answered by its ordinary refusal path.
pub(super) const WAIT_BUDGET: Duration = Duration::from_secs(10);
const MAX_WAITERS: usize = 256;
const MAX_CONFINEMENT_READS: usize = 8;

fn carrier_precheck_failed(identity: &RouteIdentity, body: &Value) -> bool {
    identity.role == tool_provider::RouteRole::ToolProviderV1
        && super::occipital_route(identity)
        && super::route_request_is_tool_call(body)
        && (super::occipital_session_refusal(identity, body).is_some()
            || super::carrier_read_only::admit(identity, body)
                .and_then(|()| {
                    super::carrier_read_only::admit_operation(
                        identity,
                        body["name"].as_str().unwrap_or(""),
                        body.get("arguments").unwrap_or(&Value::Null),
                    )
                })
                .is_err())
}

pub(super) struct DatabaseWaits {
    tasks: tokio::task::JoinSet<DecodedFrame>,
    pending: HashMap<(RouteChannel, u64), (tokio::task::AbortHandle, AdmissionWait)>,
    confinement_reads: Arc<tokio::sync::Semaphore>,
    launch_restores: Arc<tokio::sync::Semaphore>,
}

impl Default for DatabaseWaits {
    fn default() -> Self {
        Self {
            tasks: Default::default(),
            pending: Default::default(),
            confinement_reads: Arc::new(tokio::sync::Semaphore::new(MAX_CONFINEMENT_READS)),
            launch_restores: Arc::new(tokio::sync::Semaphore::new(MAX_CONFINEMENT_READS)),
        }
    }
}

impl DatabaseWaits {
    pub(super) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Compare codemode candidates only after their own saved policies have
    /// been read. Share the confinement read budget so a locked database cannot
    /// block the frame loop or create an unbounded number of blocking readers.
    pub(super) fn read_codemode_policies(
        &mut self,
        mut decoded: DecodedFrame,
        heads: Vec<RouteIdentity>,
        executor: &Executor,
    ) -> Result<(), DecodedFrame> {
        let Some(candidates) = heads
            .into_iter()
            .map(|identity| {
                executor
                    .actor_context(&identity.root)
                    .map(|ctx| (identity, ctx))
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Err(decoded);
        };
        let deadline = wait_deadline(
            &decoded,
            decoded
                .body()
                .ok()
                .and_then(|body| body.get("deadline_ms"))
                .and_then(Value::as_u64),
        );
        if deadline <= Instant::now() || self.pending.len() >= MAX_WAITERS {
            return Err(decoded);
        }
        let Ok(permit) = self.confinement_reads.clone().try_acquire_owned() else {
            return Err(decoded);
        };
        let route = route_key(decoded.frame.header.channel, decoded.frame.header.epoch);
        let key = (route, decoded.frame.header.corr);
        let admission = AdmissionWait::from_decoded(&decoded, "tool.call");
        decoded.confinement = None;
        let handle = self.tasks.spawn(async move {
            for (identity, ctx) in &candidates {
                if remote_policy::awaits_policy_store(ctx, identity) {
                    crate::database_open::dispatch(ctx);
                    ctx.wait_for_database_open(deadline).await;
                }
            }
            let read = if deadline <= Instant::now() {
                Err(remote_policy::policy_unavailable())
            } else {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        codemode::SavedPolicies::read(candidates)
                    }),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .ok_or_else(remote_policy::policy_unavailable)
            };
            decoded.codemode_policies = Some(read);
            decoded
        });
        if let Some((old, _)) = self.pending.insert(key, (handle, admission)) {
            old.abort();
        }
        Ok(())
    }

    /// Restore a revived worker's launch authority before any tool preflight,
    /// without blocking the shared reader while saved paths are checked on disk.
    pub(super) fn restore_launch(
        &mut self,
        mut decoded: DecodedFrame,
        routes: &HashMap<RouteChannel, RouteIdentity>,
        executor: &Executor,
    ) -> Result<(), DecodedFrame> {
        if decoded.launch_restore.is_some() {
            return Err(decoded);
        }
        let route = route_key(decoded.frame.header.channel, decoded.frame.header.epoch);
        let Some(identity) = routes.get(&route) else {
            return Err(decoded);
        };
        let Ok(body) = decoded.body() else {
            return Err(decoded);
        };
        // A carrier-supplied preset must never restore worker launch authority.
        if identity.role == tool_provider::RouteRole::ToolProviderV1
            && super::occipital_route(identity)
            && super::route_request_is_tool_call(body)
        {
            return Err(decoded);
        }
        let name = body.get("name").and_then(Value::as_str).unwrap_or("");
        if body.get("preset").and_then(Value::as_str) != Some("worker")
            || body
                .get("op")
                .and_then(Value::as_str)
                .is_some_and(|op| op != "tool.call")
            || tool_provider::recognized_operation(name)
            || name == "bg_events"
        {
            return Err(decoded);
        }
        // A worker run's call runs with its worker route's grants, so it
        // restores that route's launch grants, exactly as the worker's own
        // call would.
        let identity = codemode::worker_authority_route(identity);
        let Some(key) = access_grants::launch_key(identity, "worker") else {
            return Err(decoded);
        };
        let Some(ctx) = executor.actor_context(&identity.root) else {
            return Err(decoded);
        };
        let scope = crate::db::access_grants::ScopeKey::from_policy(&key);
        if ctx.launch_grants_materialized(&scope) {
            return Err(decoded);
        }
        let deadline = wait_deadline(&decoded, body.get("deadline_ms").and_then(Value::as_u64));
        let admission = AdmissionWait::from_decoded(&decoded, name);
        let permit = self.launch_restores.clone().try_acquire_owned();
        if deadline <= Instant::now() || self.pending.len() >= MAX_WAITERS || permit.is_err() {
            decoded.launch_restore = Some(Err(access_grants::restore_refusal(
                &identity.project_root,
                "admission_unavailable",
                "deadline or restoration waiter limit reached",
            )));
            return Err(decoded);
        }
        let permit = permit.unwrap();
        let restore_started = Instant::now();
        let identity = identity.clone();
        let progress = Arc::new(StdMutex::new(identity.project_root.clone()));
        let worker_progress = progress.clone();
        let key = (route, decoded.frame.header.corr);
        let handle = self.tasks.spawn(async move {
            let worker_ctx = ctx.clone();
            let result = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                tokio::task::spawn_blocking(move || {
                    // Retain the permit until even a timed-out database read
                    // exits, so stalled requests cannot grow the blocking pool.
                    let _permit = permit;
                    access_grants::restore_launch_before(&identity, &worker_ctx, "worker", deadline, worker_progress)
                }),
            ).await;
            let restored = match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(access_grants::restore_refusal(
                    &progress.lock().unwrap_or_else(|e| e.into_inner()), "worker_failed", error)),
                Err(_) => Err(access_grants::restore_refusal(
                    &progress.lock().unwrap_or_else(|e| e.into_inner()), "deadline_exceeded", "admission deadline exceeded")),
            };
            decoded.launch_restore = Some(match restored {
                Ok(crate::db::access_grants::LaunchRestore::Materialized | crate::db::access_grants::LaunchRestore::AlreadyMaterialized) => {
                    ctx.memoize_launch_grants(scope);
                    Ok(())
                }
                Ok(crate::db::access_grants::LaunchRestore::NoParams) => Ok(()),
                Err(mut error) => {
                    crate::slog_warn!("launch_grant_restore_failed: owner={} scope_ref={} epoch={} code={} error={}",
                        scope.owner, scope.scope_ref, scope.epoch, error.code, error.message);
                    if error.detail.is_none() { error.detail = Some(json!({"retryable":true,"phase":"launch_grant_restore"})); }
                    Err(error)
                }
            });
            decoded.phase_trace.note_launch_restore_wait(restore_started.elapsed());
            decoded
        });
        if let Some((old, _)) = self.pending.insert(key, (handle, admission)) {
            old.abort();
        }
        Ok(())
    }

    /// Return the frame unchanged unless this is a persistence-dependent call
    /// on a bound root whose database is opening, a catalog that needs to
    /// persist access settings, or a scoped worker's request whose saved
    /// confinement is not yet known. Failure and overflow are answered by the
    /// request's readiness refusal path without waiting.
    pub(super) fn defer(
        &mut self,
        decoded: DecodedFrame,
        routes: &HashMap<RouteChannel, RouteIdentity>,
        executor: &Executor,
    ) -> Result<(), DecodedFrame> {
        let route = route_key(decoded.frame.header.channel, decoded.frame.header.epoch);
        let Some(identity) = routes.get(&route) else {
            return Err(decoded);
        };
        let Some(ctx) = executor.actor_context(&identity.root) else {
            return Err(decoded);
        };
        let Ok(body) = decoded.body() else {
            return Err(decoded);
        };
        // Structural carrier refusals precede all policy/database waits, for
        // both the bare request and explicit tool.call forms.
        if carrier_precheck_failed(identity, body) {
            return Err(decoded);
        }
        if identity.role == tool_provider::RouteRole::ToolProviderV1
            && super::occipital_carrier_route(identity)
            && super::route_request_is_tool_call(body)
        {
            // Worker role and confinement both depend on the policy route's
            // preset row. Do not let the carrier's disables or preset reject
            // the call before that row can be read.
            if identity
                .scope
                .as_ref()
                .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Worker)
                && ctx.database_runtime_initializing()
            {
                let deadline_ms = body.get("deadline_ms").and_then(Value::as_u64);
                return self.wait(decoded, route, &ctx, "tool.call".into(), deadline_ms, true);
            }
            return Err(decoded);
        }
        // A scoped worker's saved confinement lives in aft.db, and the bind
        // reply goes out while that database may still be opening. Its
        // catalog fetch and tool calls wait for the open rather than be
        // refused for an unreadable fence (read-only tools included).
        let confine_wait = remote_policy::awaits_policy_store(&ctx, identity);
        let operation = body.get("op").and_then(Value::as_str).or_else(|| {
            body.get("name")
                .and_then(Value::as_str)
                .filter(|name| tool_provider::recognized_operation(name))
        });
        if let Some(operation) =
            operation.filter(|operation| tool_provider::recognized_operation(operation))
        {
            // Access settings and scoped catalog preset attestations also use
            // aft.db, even when no saved worker policy needs to be read.
            let catalog_wait = operation == "tool.catalog"
                && ctx.database_runtime_initializing()
                && remote_policy::catalog_needs_database(body, identity);
            if catalog_wait || (confine_wait && matches!(operation, "tool.catalog" | "tool.call")) {
                let name = operation.to_string();
                let deadline_ms = body.get("deadline_ms").and_then(Value::as_u64);
                return self.wait(decoded, route, &ctx, name, deadline_ms, true);
            }
            return Err(decoded);
        }
        if identity.role == tool_provider::RouteRole::ToolProviderV1 {
            let Ok(call) = serde_json::from_value::<
                cortexkit_role_tool_provider::call::ToolCallRequest,
            >(body.clone()) else {
                return Err(decoded);
            };
            if tool_provider::admit(
                &call,
                identity.scope.is_some(),
                &identity.disabled_tools,
                crate::bash_background::powershell_available(),
                &identity.session,
                !matches!(identity.trust, BindTrust::Untrusted),
            )
            .is_err()
            {
                return Err(decoded);
            }
        }
        let Some(name) = body
            .get("name")
            .or_else(|| body.get("op"))
            .and_then(Value::as_str)
        else {
            return Err(decoded);
        };
        if crate::tool_gate::refusal(
            "database-admission",
            name,
            body.get("arguments").unwrap_or(&Value::Null),
            &identity.disabled_tools,
        )
        .is_some()
        {
            return Err(decoded);
        }
        if ctx.claim_database_runtime_retry(name) {
            let generation = ctx.configure_generation();
            let response = executor.submit_maintenance_async(
                identity.root.clone(),
                crate::executor::Lane::MaintenanceCommit,
                format!("database-retry-{generation}"),
                Box::new(move |ctx| {
                    // Serialize pool installation with config-changing binds, just
                    // like the initial deferred open. A newer bind owns its retry.
                    if ctx.configure_generation() == generation {
                        ctx.retry_database_runtime();
                    }
                    crate::protocol::Response::success("database-retry", serde_json::json!({}))
                }),
            );
            let retry_ctx = ctx.clone();
            tokio::spawn(async move {
                if !response.await.is_ok_and(|response| response.success)
                    && retry_ctx.configure_generation() == generation
                {
                    retry_ctx.finish_database_runtime_error(
                        "Database retry admission failed".into(),
                        true,
                    );
                }
            });
        }
        if !ctx.database_runtime_pending(name) && !confine_wait {
            return Err(decoded);
        }
        let name = name.to_string();
        let deadline_ms = body.get("deadline_ms").and_then(Value::as_u64);
        self.wait(decoded, route, &ctx, name, deadline_ms, confine_wait)
    }

    /// Read a scoped worker route's saved confinement on a blocking thread
    /// before one of its tool calls reaches admission, when memory cannot
    /// answer it. The read takes the aft.db lock, which a catalog job can
    /// hold for seconds while SQLite waits for its write lock; taking it on
    /// the frame loop would stop health checks, cancellations and other roots
    /// from being served meanwhile. The frame comes back from [`Self::next`]
    /// carrying the result in `DecodedFrame::confinement`, and the call is
    /// not admitted before then. Opening the database and reading its policy
    /// share the bounded persistence wait budget. Memory answers are carried
    /// forward too, since a later nonblocking probe may find a busy slot.
    /// Past the waiter limit the frame comes back unchanged, and
    /// admission refuses it for want of a policy (see
    /// `remote_policy::admit_confined_on_frame_loop`).
    pub(super) fn read_confinement(
        &mut self,
        mut decoded: DecodedFrame,
        routes: &HashMap<RouteChannel, RouteIdentity>,
        executor: &Executor,
    ) -> Result<(), DecodedFrame> {
        if decoded.confinement.is_some() {
            return Err(decoded);
        }
        let route = route_key(decoded.frame.header.channel, decoded.frame.header.epoch);
        let Some(identity) = routes.get(&route) else {
            return Err(decoded);
        };
        let Some(ctx) = executor.actor_context(&identity.root) else {
            return Err(decoded);
        };
        let Ok(body) = decoded.body() else {
            return Err(decoded);
        };
        if carrier_precheck_failed(identity, body) {
            return Err(decoded);
        }
        // V1 carrier policy is resolved on its executor job, including saved
        // confinement. Legacy calls still need the ordinary pre-admission read.
        if identity.role == tool_provider::RouteRole::ToolProviderV1
            && super::occipital_carrier_route(identity)
        {
            return Err(decoded);
        }
        // Catalog fetches and the other provider operations run on the
        // executor and read the policy there; only tool calls are checked
        // against it on the frame loop.
        let operation = body.get("op").and_then(Value::as_str).or_else(|| {
            body.get("name")
                .and_then(Value::as_str)
                .filter(|name| tool_provider::recognized_operation(name))
        });
        if operation.is_some_and(|operation| {
            tool_provider::recognized_operation(operation) && operation != "tool.call"
        }) {
            return Err(decoded);
        }
        if identity.role == tool_provider::RouteRole::ToolProviderV1 {
            if let Ok(call) = serde_json::from_value::<
                cortexkit_role_tool_provider::call::ToolCallRequest,
            >(body.clone())
            {
                if tool_provider::shell_capability_refusal(
                    &call.name,
                    tool_provider::admission_trusted(identity, &call),
                )
                .is_some()
                {
                    return Err(decoded);
                }
            }
        }
        let opening = remote_policy::awaits_policy_store(&ctx, identity);
        if !opening {
            if let Some(read) = remote_policy::CarriedConfinement::from_memory(&ctx, identity) {
                // Keep the answer for admission: another route may hold the
                // handle slot by then, making the same memory probe inconclusive.
                decoded.confinement = Some(read);
                return Err(decoded);
            }
        }
        let name = body
            .get("name")
            .or_else(|| body.get("op"))
            .and_then(Value::as_str)
            .unwrap_or("tool.call")
            .to_owned();
        let deadline = wait_deadline(&decoded, body.get("deadline_ms").and_then(Value::as_u64));
        if deadline <= Instant::now() {
            decoded.confinement = Some(remote_policy::CarriedConfinement::unavailable());
            return Err(decoded);
        }
        if self.pending.len() >= MAX_WAITERS {
            return Err(decoded);
        }
        let Ok(permit) = self.confinement_reads.clone().try_acquire_owned() else {
            return Err(decoded);
        };
        let key = (route, decoded.frame.header.corr);
        let admission = AdmissionWait::from_decoded(&decoded, &name);
        let identity = identity.clone();
        if opening {
            crate::database_open::dispatch(&ctx);
        }
        let handle = self.tasks.spawn(async move {
            // Keyed calls skip the general persistence wait because the ledger
            // owns admission, but their policy still needs a completed open.
            if opening {
                ctx.wait_for_database_open(deadline).await;
            }
            if deadline <= Instant::now() {
                decoded.confinement = Some(remote_policy::CarriedConfinement::unavailable());
                return decoded;
            }
            let read = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                tokio::task::spawn_blocking(move || {
                    // A timed-out or cancelled waiter cannot stop this blocking
                    // read. Count it until it finishes, including time queued
                    // in Tokio's blocking pool or waiting for the database slot.
                    let _permit = permit;
                    remote_policy::CarriedConfinement::read(&ctx, &identity)
                }),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(remote_policy::CarriedConfinement::unavailable);
            decoded.confinement = Some(read);
            decoded
        });
        if let Some((old, _)) = self.pending.insert(key, (handle, admission)) {
            old.abort();
        }
        Ok(())
    }

    /// Hold `decoded` until the root's database open finishes or the wait
    /// budget ends. With `any_command` false the wait ends as soon as `name`
    /// no longer needs to wait for persistence; with it true (calls whose
    /// saved confinement must be read first) the wait lasts until the open
    /// itself finishes, whatever tool `name` is.
    fn wait(
        &mut self,
        decoded: DecodedFrame,
        route: RouteChannel,
        ctx: &Arc<AppContext>,
        name: String,
        deadline_ms: Option<u64>,
        any_command: bool,
    ) -> Result<(), DecodedFrame> {
        // A root reported as initializing always has its open staged or
        // running. Hand a staged open to the database-open thread now rather
        // than trusting that the bind's dispatch or the configure tail will
        // get to it soon: the tail can be queued behind other roots for
        // minutes, and a missed dispatch must not leave this root refusing
        // mutations indefinitely. A no-op when the open is already running.
        crate::database_open::dispatch(ctx);
        if self.pending.len() >= MAX_WAITERS {
            return Err(decoded);
        }
        let key = (route, decoded.frame.header.corr);
        let admission = AdmissionWait::from_decoded(&decoded, &name);
        // A caller may shorten the wait with an absolute Unix-millisecond
        // deadline. The bash timeout limits command execution, not the wait
        // for permission to admit the command.
        let deadline = wait_deadline(&decoded, deadline_ms);
        let ctx = Arc::clone(ctx);
        let handle = self.tasks.spawn(async move {
            if any_command {
                ctx.wait_for_database_open(deadline).await;
            } else {
                ctx.wait_for_database_runtime(&name, deadline).await;
            }
            #[cfg(feature = "test-timing-hooks")]
            if name == "bg_events" {
                // Pause a resumed subscription without holding configure's
                // maintenance job, so route reclamation can finish first.
                if let Some(gate) = std::env::var_os("AFT_TEST_BG_EVENTS_DATABASE_RESUME_GATE")
                    .map(std::path::PathBuf::from)
                {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while gate.exists() {
                        assert!(
                            Instant::now() < deadline,
                            "bg_events resume gate not released"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            decoded
        });
        if let Some((old, _)) = self.pending.insert(key, (handle, admission)) {
            old.abort();
        }
        Ok(())
    }

    pub(super) async fn next(&mut self) -> DecodedFrame {
        loop {
            match self.tasks.join_next_with_id().await {
                Some(Ok((id, decoded))) => {
                    let key = (
                        route_key(decoded.frame.header.channel, decoded.frame.header.epoch),
                        decoded.frame.header.corr,
                    );
                    // A completed task can survive abort in the JoinSet. Only
                    // the currently tracked task may resume this correlation.
                    if !self
                        .pending
                        .get(&key)
                        .is_some_and(|(handle, _)| handle.id() == id)
                    {
                        continue;
                    }
                    self.pending.remove(&key);
                    return decoded;
                }
                Some(Err(_)) => continue,
                None => std::future::pending::<()>().await,
            }
        }
    }

    pub(super) fn cancel(&mut self, route: RouteChannel, corr: u64) -> Option<RequestFrameMeta> {
        let (handle, meta) = self.pending.remove(&(route, corr))?;
        handle.abort();
        Some(meta.meta)
    }

    pub(super) fn held_since(&self) -> Vec<Instant> {
        self.pending
            .values()
            .map(|(_, wait)| wait.held_since)
            .collect()
    }

    pub(super) fn drain_for_module_drain(&mut self) -> Result<Vec<Frame>, SubcError> {
        self.tasks.abort_all();
        self.pending
            .drain()
            .map(|(_, (task, wait))| {
                task.abort();
                wait.draining_frame()
            })
            .collect()
    }
}

fn wait_deadline(decoded: &DecodedFrame, deadline_ms: Option<u64>) -> Instant {
    let mut deadline = decoded.phase_trace.received_at() + WAIT_BUDGET;
    if let Some(epoch_ms) = deadline_ms {
        let remaining = Duration::from_millis(
            epoch_ms.saturating_sub(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            ),
        );
        deadline = deadline.min(Instant::now() + remaining.min(WAIT_BUDGET));
    }
    deadline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn module_drain_counts_and_answers_database_admission_waits() {
        let (_dir, root) = test_support::test_root("drain-database-waits");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Executor::new();
        executor.register_actor(root.clone(), ctx);
        let route = route_key(42, 1);
        let routes = HashMap::from([(route, test_support::route_identity(&root, "session"))]);
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                42,
                1,
                7,
                serde_json::to_vec(&json!({"name":"bash","arguments":{}})).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = DatabaseWaits::default();
        assert!(waits.defer(decoded, &routes, &executor).is_ok());
        let (edge, _) = tool_provider::LedgerEdge::new();
        let census = drain::held_request_census(
            &Arc::default(),
            &PendingSubcResponses::default(),
            &DispatchPathMetrics::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            0,
            &waits,
            &edge,
        );
        assert_eq!(census.total(), 1);
        let frames = waits.drain_for_module_drain().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.corr, 7);
        let body: Value = serde_json::from_slice(&frames[0].body).unwrap();
        assert_eq!(body["code"], "module_reloading");
        assert!(waits.pending.is_empty());
        assert!(waits.cancel(route, 7).is_none());
    }

    #[tokio::test]
    async fn bg_events_subscriptions_skip_database_initialization() {
        let (_dir, root) = test_support::test_root("bg-events-database-waits");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let routes = [121, 122]
            .into_iter()
            .map(|channel| {
                (
                    route_key(channel, 1),
                    test_support::route_identity(&root, "session"),
                )
            })
            .collect();
        let mut waits = DatabaseWaits::default();
        for channel in [121, 122] {
            let decoded = DecodedFrame {
                parsed_body: Default::default(),
                confinement: None,
                launch_restore: None,
                codemode_policies: None,
                frame: Frame::build(
                    FrameType::Request,
                    control_flags(),
                    channel,
                    1,
                    950,
                    serde_json::to_vec(&json!({"op": "bg_events"})).unwrap(),
                )
                .unwrap(),
                phase_trace: PhaseTrace::new(Instant::now()),
            };
            assert!(
                waits.defer(decoded, &routes, &executor).is_err(),
                "bg_events on channel {channel} must reach its inline subscription handler"
            );
        }
        assert!(waits.is_empty());
        assert!(ctx.database_runtime_initializing());
        assert!(ctx.database_runtime_pending("unclassified_test_operation"));
    }

    #[tokio::test]
    async fn stale_database_completion_cannot_remove_replacement_waiter() {
        let route = route_key(42, 1);
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(FrameType::Request, control_flags(), 42, 1, 1, Vec::new()).unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = DatabaseWaits::default();
        let admission = AdmissionWait::from_decoded(&decoded, "bash");
        let old = waits.tasks.spawn(async move { decoded });
        tokio::task::yield_now().await;
        assert!(old.is_finished());
        let replacement = waits.tasks.spawn(std::future::pending());
        waits.pending.insert((route, 1), (replacement, admission));
        old.abort();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), waits.next())
                .await
                .is_err()
        );
        assert!(waits.cancel(route, 1).is_some());
    }

    #[tokio::test]
    async fn bash_status_waits_for_database_initialization_but_not_failure() {
        let (_dir, root) = test_support::test_root("bash-status-database-waits");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let route = route_key(42, 1);
        let routes = HashMap::from([(route, test_support::route_identity(&root, "test"))]);
        let frame = |name: &str, corr, deadline_ms| DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                42,
                1,
                corr,
                serde_json::to_vec(&json!({
                    "name": name, "arguments": {}, "deadline_ms": deadline_ms,
                }))
                .unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = DatabaseWaits::default();
        for name in ["bash_status", "aft_bash_status"] {
            assert!(waits
                .defer(frame(name, 1, None::<u64>), &routes, &executor)
                .is_ok());
            assert!(
                tokio::time::timeout(Duration::from_millis(30), waits.next())
                    .await
                    .is_err()
            );
            assert!(waits.cancel(route, 1).is_some());
        }

        // An expired caller deadline releases the request even if the open
        // never finishes; the handler can then try its finished-bundle fallback.
        assert!(waits
            .defer(frame("bash_status", 2, Some(0)), &routes, &executor)
            .is_ok());
        let resumed = tokio::time::timeout(Duration::from_millis(500), waits.next())
            .await
            .unwrap();
        assert_eq!(resumed.frame.header.corr, 2);
        assert!(ctx.database_runtime_pending("bash_status"));
        assert!(ctx
            .database_runtime_refusal("status", "bash_status")
            .is_none());

        assert!(waits
            .defer(frame("bash_status", 3, None), &routes, &executor)
            .is_ok());
        ctx.finish_database_runtime_error("database cannot open".into(), false);
        let resumed = tokio::time::timeout(Duration::from_millis(500), waits.next())
            .await
            .unwrap();
        assert_eq!(resumed.frame.header.corr, 3);
        assert!(ctx
            .database_runtime_refusal("status", "bash_status")
            .is_none());
        assert!(waits
            .defer(frame("bash_status", 4, None), &routes, &executor)
            .is_err());
        assert!(waits.is_empty());
    }

    /// A scoped worker route on `root` whose saved confinement lives in
    /// aft.db, and an unscoped head route on the same root.
    fn scoped_worker_and_head(root: &ProjectRootId) -> (RouteIdentity, RouteIdentity) {
        let unscoped = test_support::route_identity(root, "head");
        let scope: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({
            "owner": {"kind": "direct"}, "ref": "worker-scope", "scope_epoch": 1,
            "kind": "worker", "owner_authorized": true,
        }))
        .unwrap();
        let worker = RouteIdentity(Arc::new(RouteIdentityData {
            root: root.clone(),
            project_root: unscoped.project_root.clone(),
            harness: unscoped.harness.clone(),
            session: "worker".into(),
            role: tool_provider::RouteRole::Legacy,
            trust: BindTrust::FirstParty,
            spawn_principal: unscoped.spawn_principal.clone(),
            consumer_elicitation_capable: false,
            tool_result_body: false,
            disabled_tools: Arc::default(),
            scope: Some(scope),
            call_mode: subc_protocol::call_mode::CallMode::Classic,
            run_scope: OnceLock::new(),
            made_tool_call: AtomicBool::new(false),
            run_owner: None,
        }));
        assert!(remote_policy::key(&worker, Some("worker")).is_some());
        (worker, unscoped)
    }

    fn head_catalog(
        root: &ProjectRootId,
        deadline_ms: Option<u64>,
    ) -> (RouteIdentity, DecodedFrame) {
        let (mut head, _) = scoped_worker_and_head(root);
        let scope = Arc::get_mut(&mut head.0).unwrap().scope.as_mut().unwrap();
        scope.kind = subc_protocol::scope::ScopeKind::Head;
        scope.owner = Principal::Reserved {
            module_id: "prefrontal-core".into(),
        };
        if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
            &mut Arc::get_mut(&mut head.0).unwrap().spawn_principal
        {
            *principal_id = Some("reserved:prefrontal-core".into());
        }
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                43,
                1,
                7,
                serde_json::to_vec(&json!({
                    "op": "tool.catalog", "preset": "head", "params": {"mode": "ask"},
                    "deadline_ms": deadline_ms,
                }))
                .unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        (head, decoded)
    }

    async fn head_catalog_reply(
        decoded: DecodedFrame,
        identity: RouteIdentity,
        executor: &Arc<Executor>,
    ) -> Frame {
        let (writer, mut replies) = mpsc::channel(8);
        let body = decoded.body().unwrap().clone();
        submit_provider_read(
            &writer,
            &decoded.frame,
            identity,
            executor,
            &Arc::default(),
            &Arc::new(DispatchPathMetrics::new()),
            "tool.catalog",
            body,
            decoded.phase_trace,
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .expect("catalog reply")
            .unwrap()
            .frame
    }

    #[tokio::test]
    async fn head_catalog_waits_for_database_and_persists_mode() {
        use std::future::Future;
        let (dir, root) = test_support::test_root("head-catalog-database-wait");
        let ctx = test_support::test_ctx();
        // Bind acknowledges a configured root before the deferred open installs
        // its handle. Hold that state explicitly, without racing a real open.
        ctx.mark_database_runtime_initializing_for_test();
        assert!(ctx.db().is_none());
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        let (head, decoded) = head_catalog(&root, None);
        let routes = HashMap::from([(route_key(43, 1), head.clone())]);
        let mut waits = DatabaseWaits::default();
        let decoded = match waits.defer(decoded, &routes, &executor) {
            Ok(()) => {
                let resumed = waits.next();
                tokio::pin!(resumed);
                tokio::task::yield_now().await;
                std::future::poll_fn(|cx| {
                    assert!(
                        resumed.as_mut().poll(cx).is_pending(),
                        "catalog resumed before database open"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                ctx.set_db(Arc::new(std::sync::Mutex::new(
                    crate::db::open(&dir.path().join("aft.db")).unwrap(),
                )));
                ctx.finish_database_runtime(Ok(()));
                resumed.await
            }
            Err(decoded) => decoded,
        };
        let reply = head_catalog_reply(decoded, head.clone(), &executor).await;
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        let db = ctx.db().expect("successful catalog requires persistence");
        let conn = db.lock().unwrap();
        let key = access_grants::policy_scope_key(&head);
        assert!(crate::db::access_grants::is_head(&conn, &key).unwrap());
        assert_eq!(
            crate::db::access_grants::mode(&conn, &key).unwrap(),
            crate::access_policy::ApprovalMode::Ask
        );
    }

    #[tokio::test]
    async fn rich_ui_preset_writer_catalog_waits_for_open_then_records_without_refetch() {
        use std::future::Future;
        for preset in ["worker", "reader"] {
            let (dir, root) = test_support::test_root("preset-catalog-database-wait");
            let ctx = test_support::test_ctx();
            ctx.mark_database_runtime_initializing_for_test();
            let executor = Arc::new(Executor::new());
            assert!(executor.register_actor(root.clone(), ctx.clone()));
            let (mut identity, mut decoded) = head_catalog(&root, None);
            let data = Arc::get_mut(&mut identity.0).unwrap();
            data.role = tool_provider::RouteRole::ToolProviderV1;
            if preset == "worker" {
                data.scope.as_mut().unwrap().kind = subc_protocol::scope::ScopeKind::Worker;
            }
            if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
                &mut data.spawn_principal
            {
                *principal_id = Some("reserved:broca".into());
            }
            decoded.frame = Frame::build(
                FrameType::Request,
                control_flags(),
                43,
                1,
                7,
                serde_json::to_vec(&json!({"op":"tool.catalog","preset":preset})).unwrap(),
            )
            .unwrap();
            let routes = HashMap::from([(route_key(43, 1), identity.clone())]);
            let mut waits = DatabaseWaits::default();
            assert!(
                waits.defer(decoded, &routes, &executor).is_ok(),
                "{preset} catalog must wait"
            );
            let resumed = waits.next();
            tokio::pin!(resumed);
            tokio::task::yield_now().await;
            std::future::poll_fn(|cx| {
                assert!(
                    resumed.as_mut().poll(cx).is_pending(),
                    "catalog resumed before open"
                );
                std::task::Poll::Ready(())
            })
            .await;
            ctx.set_db(Arc::new(std::sync::Mutex::new(
                crate::db::open(&dir.path().join("aft.db")).unwrap(),
            )));
            ctx.finish_database_runtime(Ok(()));
            let reply = head_catalog_reply(resumed.await, identity.clone(), &executor).await;
            assert_eq!(
                reply.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            let key = remote_policy::session_preset_key(&identity, preset).unwrap();
            assert_eq!(
                crate::db::rich_ui::session_presets(&ctx.db().unwrap().lock().unwrap(), &key)
                    .unwrap(),
                [preset]
            );
        }
    }

    #[tokio::test]
    async fn head_catalog_database_deadline_and_failure_are_retryable() {
        let (_dir, root) = test_support::test_root("head-catalog-database-refusal");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        for code in ["database_initializing", "database_unavailable"] {
            let (head, decoded) = head_catalog(&root, Some(0));
            let routes = HashMap::from([(route_key(43, 1), head.clone())]);
            let mut waits = DatabaseWaits::default();
            let decoded = match waits.defer(decoded, &routes, &executor) {
                Ok(()) => waits.next().await,
                Err(decoded) => decoded,
            };
            let reply = head_catalog_reply(decoded, head, &executor).await;
            assert_eq!(reply.header.ty, FrameType::Error);
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(body["code"], code, "{body}");
            assert_eq!(body["detail"]["retryable"], true, "{body}");
            assert!(ctx.db().is_none());
            ctx.finish_database_runtime_error("injected database busy failure".into(), true);
        }
    }

    #[tokio::test]
    async fn catalog_database_requirement_tracks_access_settings_not_head_worker_previews() {
        let (_dir, root) = test_support::test_root("head-catalog-database-parameters");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        for (preset, params, needs_database) in [
            ("head", json!({"mode": "ask"}), true),
            ("worker", json!({"allow_paths": []}), true),
            ("worker", json!({"mode": "ask"}), false),
            ("worker", json!({"remote_exec": {"enabled": false}}), false),
            ("head", json!({}), false),
        ] {
            let (head, mut decoded) = head_catalog(&root, None);
            let body = json!({"op": "tool.catalog", "preset": preset, "params": params});
            decoded.frame = Frame::build(
                FrameType::Request,
                control_flags(),
                43,
                1,
                7,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            let routes = HashMap::from([(route_key(43, 1), head.clone())]);
            let mut waits = DatabaseWaits::default();
            match waits.defer(decoded, &routes, &executor) {
                Ok(()) => assert!(needs_database, "preview waited for persistence: {body}"),
                Err(decoded) => {
                    assert!(
                        !needs_database,
                        "access settings bypassed persistence: {body}"
                    );
                    let reply = head_catalog_reply(decoded, head, &executor).await;
                    assert_eq!(
                        reply.header.ty,
                        FrameType::Response,
                        "{}",
                        String::from_utf8_lossy(&reply.body)
                    );
                }
            }
            assert!(ctx.db().is_none());
        }
    }

    #[tokio::test]
    async fn scoped_worker_calls_wait_for_opening_database_then_run_unconfined() {
        let (dir, root) = test_support::test_root("confine-database-waits");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let (worker, unscoped) = scoped_worker_and_head(&root);
        let (scoped_route, unscoped_route) = (route_key(43, 1), route_key(42, 1));
        let routes = HashMap::from([(scoped_route, worker.clone()), (unscoped_route, unscoped)]);
        let frame = |channel: u16, corr, body: Value| DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                channel,
                1,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let read = json!({"name": "read", "preset": "worker", "arguments": {"path": "a"}});
        let mut waits = DatabaseWaits::default();
        // Read-only tools never waited for the database, and still do not on
        // a route that can hold no saved confinement.
        assert!(waits
            .defer(frame(42, 1, read.clone()), &routes, &executor)
            .is_err());
        assert!(waits
            .defer(
                frame(43, 2, json!({"op": "tool.withdraw"})),
                &routes,
                &executor
            )
            .is_err());
        assert!(waits.defer(frame(43, 3, read), &routes, &executor).is_ok());
        assert!(waits
            .defer(
                frame(43, 4, json!({"op": "tool.catalog", "preset": "worker"})),
                &routes,
                &executor
            )
            .is_ok());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), waits.next())
                .await
                .is_err(),
            "a scoped call resumed before the database opened"
        );
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        ctx.finish_database_runtime(Ok(()));
        let mut resumed = Vec::new();
        for _ in 0..2 {
            let decoded = tokio::time::timeout(Duration::from_millis(500), waits.next())
                .await
                .unwrap();
            resumed.push(decoded.frame.header.corr);
        }
        resumed.sort_unstable();
        assert_eq!(resumed, [3, 4]);
        // Once open, the saved policy answers: this worker never asked for
        // confinement, so its read is admitted as before.
        assert!(!remote_policy::admit_confined(&ctx, &worker, "read").unwrap());
        assert!(waits.is_empty());
    }

    #[tokio::test]
    async fn codemode_saved_policies_wait_off_the_frame_loop() {
        let (dir, root) = test_support::test_root("codemode-policy-held-lock");
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let (worker, head) = scoped_worker_and_head(&root);
        let candidates = vec![worker, head];
        let db = ctx.db().unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _conn = db.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut waits = DatabaseWaits::default();
        let started = Instant::now();
        assert!(waits
            .read_codemode_policies(
                policy_read_frame(1, crate::bash_background::persistence::unix_millis() + 5000),
                candidates.clone(),
                &executor,
            )
            .is_ok());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), waits.next())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let resumed = tokio::time::timeout(Duration::from_secs(5), waits.next())
            .await
            .unwrap();
        let Some(Ok(policies)) = resumed.codemode_policies else {
            panic!("saved policies were not read");
        };
        for candidate in &candidates {
            assert_eq!(
                policies.get(&ctx, candidate),
                Some(Ok(codemode::EffectiveSavedPolicy::default()))
            );
        }
        assert!(waits.is_empty());
    }

    #[tokio::test]
    async fn confinement_read_runs_off_the_frame_loop_and_holds_admission_until_it_finishes() {
        let (dir, root) = test_support::test_root("confine-read-held-lock");
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let (worker, _) = scoped_worker_and_head(&root);
        let db = ctx.db().unwrap();
        // Saved straight into aft.db, as a catalog fetch in an earlier process
        // would have left it, so memory does not know this worker is confined.
        crate::db::remote_exec::freeze(
            &db.lock().unwrap(),
            &remote_policy::key(&worker, Some("worker")).unwrap(),
            &crate::exec_remote::FrozenParams {
                confine: Some(crate::exec_remote::RootConfinement::Root),
                ..Default::default()
            },
            crate::bash_background::persistence::unix_millis(),
            || {},
        )
        .unwrap();
        assert!(remote_policy::confinement_from_memory(&ctx, &worker).is_none());
        let routes = HashMap::from([(route_key(43, 1), worker.clone())]);
        let frame = |corr, body: Value| DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                43,
                1,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };

        // Hold the aft.db lock on another thread, as a catalog job's policy
        // freeze does while SQLite waits for its write lock. The holder lets
        // go on its own after a bound, so code that blocks on the lock fails
        // the timing checks below instead of hanging the test.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder_db = Arc::clone(&db);
        let holder = std::thread::spawn(move || {
            let _conn = holder_db.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        // Frame-loop admission never reads aft.db itself: with no read result
        // it refuses at once instead of waiting, or running the call unconfined.
        assert_eq!(
            remote_policy::admit_confined_on_frame_loop(&ctx, &worker, "read", None)
                .unwrap_err()
                .code,
            "confine_policy_unavailable"
        );
        let mut waits = DatabaseWaits::default();
        // A catalog fetch reads the policy on the executor, not here.
        assert!(waits
            .read_confinement(
                frame(1, json!({"op": "tool.catalog", "preset": "worker"})),
                &routes,
                &executor
            )
            .is_err());
        let bash = json!({"name": "bash", "preset": "worker", "arguments": {"command": "true"}});
        assert!(waits
            .read_confinement(frame(2, bash), &routes, &executor)
            .is_ok());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "frame-loop admission waited {:?} for the held aft.db lock",
            started.elapsed()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), waits.next())
                .await
                .is_err(),
            "a call resumed before its confinement could be read"
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let resumed = tokio::time::timeout(Duration::from_secs(5), waits.next())
            .await
            .unwrap();
        assert_eq!(resumed.frame.header.corr, 2);
        assert_eq!(
            resumed.confinement.as_ref().map(|read| &read.read),
            Some(&Ok(true))
        );
        assert_eq!(
            remote_policy::admit_confined_on_frame_loop(
                &ctx,
                &worker,
                "bash",
                resumed.confinement.as_ref()
            )
            .unwrap_err()
            .code,
            "tool_not_offered_confined"
        );
        // The confined answer is now remembered, so later calls need no read.
        let read = json!({"name": "read", "preset": "worker", "arguments": {"path": "a"}});
        assert!(waits
            .read_confinement(frame(3, read), &routes, &executor)
            .is_err());
        assert!(remote_policy::admit_confined_on_frame_loop(&ctx, &worker, "read", None).unwrap());
        assert!(waits.is_empty());
    }

    #[tokio::test]
    async fn unconfined_worker_is_read_from_aft_db_once_then_answered_from_memory() {
        let (dir, root) = test_support::test_root("unconfined-memo");
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let (worker, _) = scoped_worker_and_head(&root);
        let routes = HashMap::from([(route_key(43, 1), worker.clone())]);
        let frame = |corr| DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                43,
                1,
                corr,
                serde_json::to_vec(&json!({
                    "name": "read", "preset": "worker", "arguments": {"path": "a"},
                }))
                .unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = DatabaseWaits::default();
        // The first call reads this worker's policy from aft.db: none saved.
        assert!(waits.read_confinement(frame(1), &routes, &executor).is_ok());
        let resumed = tokio::time::timeout(Duration::from_secs(5), waits.next())
            .await
            .unwrap();
        assert_eq!(
            resumed.confinement.as_ref().map(|read| &read.read),
            Some(&Ok(false))
        );
        assert_eq!(ctx.policy_confinement_reads_for_test(), 1);

        // Hold the aft.db lock, as a catalog fetch saving a worker's policy
        // does while SQLite waits for its write lock. The holder lets go after
        // at most five seconds, so code that blocks on the lock fails the
        // timing check instead of hanging.
        let db = ctx.db().unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _conn = db.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        // The second call is answered from memory, on the frame loop and on
        // the executor alike, without reading aft.db or waiting for its lock.
        let second = match waits.read_confinement(frame(2), &routes, &executor) {
            Ok(()) => panic!("a remembered unconfined worker was read from aft.db again"),
            Err(decoded) => decoded,
        };
        assert_eq!(
            second.confinement.as_ref().map(|read| &read.read),
            Some(&Ok(false))
        );
        assert!(!remote_policy::admit_confined_on_frame_loop(&ctx, &worker, "bash", None).unwrap());
        assert!(!remote_policy::admit_confined(&ctx, &worker, "bash").unwrap());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a remembered unconfined worker waited {:?} for the aft.db lock",
            started.elapsed()
        );
        assert_eq!(ctx.policy_confinement_reads_for_test(), 1);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(waits.is_empty());
    }

    fn policy_read_frame(corr: u64, deadline_ms: u64) -> DecodedFrame {
        DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                43,
                1,
                corr,
                serde_json::to_vec(&json!({"name":"bash", "arguments":{"command":"true"},
                    "preset":"worker", "deadline_ms":deadline_ms}))
                .unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        }
    }

    #[tokio::test]
    async fn confinement_reads_refuse_over_limit_without_spawning() {
        check_confinement_read_limit(false).await;
    }

    #[tokio::test]
    async fn timed_out_confinement_reads_keep_permits_until_blocking_reads_finish() {
        check_confinement_read_limit(true).await;
    }

    async fn check_confinement_read_limit(after_timeout: bool) {
        let (dir, root) = test_support::test_root("confinement-read-limit");
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        let (worker, _) = scoped_worker_and_head(&root);
        let routes = HashMap::from([(route_key(43, 1), worker.clone())]);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let app = ctx.app();
        let holder = std::thread::spawn(move || {
            app.with_database_slot_locked_for_test(|| {
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            });
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut waits = DatabaseWaits::default();
        let deadline = crate::bash_background::persistence::unix_millis() + 1_000;
        for corr in 0..MAX_CONFINEMENT_READS as u64 {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            ctx.after_next_policy_memory_miss_for_test(move || {
                let _ = started_tx.send(());
            });
            assert!(waits
                .read_confinement(policy_read_frame(corr, deadline), &routes, &executor)
                .is_ok());
            tokio::time::timeout(Duration::from_secs(1), started_rx)
                .await
                .unwrap()
                .unwrap();
        }
        if after_timeout {
            for _ in 0..MAX_CONFINEMENT_READS {
                let decoded = tokio::time::timeout(Duration::from_secs(2), waits.next())
                    .await
                    .unwrap();
                assert_eq!(
                    remote_policy::admit_confined_on_frame_loop(
                        &ctx,
                        &worker,
                        "bash",
                        decoded.confinement.as_ref()
                    )
                    .unwrap_err()
                    .code,
                    "confine_policy_unavailable"
                );
            }
            assert!(waits.pending.is_empty());
        }
        // All reads reached the busy slot. Even after their callers time out,
        // another frame must not queue more blocking work behind that slot.
        let (spawned_tx, mut spawned_rx) = tokio::sync::oneshot::channel();
        ctx.after_next_policy_memory_miss_for_test(move || {
            let _ = spawned_tx.send(());
        });
        let started = Instant::now();
        let refused = waits
            .read_confinement(
                policy_read_frame(MAX_CONFINEMENT_READS as u64, deadline + 5_000),
                &routes,
                &executor,
            )
            .err()
            .expect("over-limit policy read was spawned");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            remote_policy::admit_confined_on_frame_loop(
                &ctx,
                &worker,
                "bash",
                refused.confinement.as_ref()
            )
            .unwrap_err()
            .code,
            "confine_policy_unavailable"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut spawned_rx)
                .await
                .is_err()
        );
        assert_eq!(waits.confinement_reads.available_permits(), 0);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let permits = tokio::time::timeout(
            Duration::from_secs(2),
            waits
                .confinement_reads
                .clone()
                .acquire_many_owned(MAX_CONFINEMENT_READS as u32),
        )
        .await
        .expect("finished reads leaked their permits")
        .unwrap();
        drop(permits);
        assert_eq!(
            waits.confinement_reads.available_permits(),
            MAX_CONFINEMENT_READS
        );
    }

    #[tokio::test]
    async fn expired_confinement_read_deadline_does_not_spawn() {
        let (dir, root) = test_support::test_root("expired-confinement-read");
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        let (worker, _) = scoped_worker_and_head(&root);
        let routes = HashMap::from([(route_key(43, 1), worker.clone())]);
        let (spawned_tx, mut spawned_rx) = tokio::sync::oneshot::channel();
        ctx.after_next_policy_memory_miss_for_test(move || {
            let _ = spawned_tx.send(());
        });
        let mut waits = DatabaseWaits::default();
        let decoded = waits
            .read_confinement(policy_read_frame(1, 1), &routes, &executor)
            .err()
            .expect("an expired frame queued a policy read");
        assert!(waits.is_empty());
        assert_eq!(
            waits.confinement_reads.available_permits(),
            MAX_CONFINEMENT_READS
        );
        assert!(spawned_rx.try_recv().is_err());
        assert_eq!(ctx.policy_confinement_reads_for_test(), 0);
        assert_eq!(
            remote_policy::admit_confined_on_frame_loop(
                &ctx,
                &worker,
                "bash",
                decoded.confinement.as_ref()
            )
            .unwrap_err()
            .code,
            "confine_policy_unavailable"
        );
    }

    #[tokio::test]
    async fn database_wait_notification_releases_only_persistence_calls_and_cancel_aborts_wait() {
        let (_dir, root) = test_support::test_root("database-waits");
        let ctx = test_support::test_ctx();
        // Initializing with no staged open, so nothing finishes the open
        // behind this test's back and only `finish_database_runtime` below
        // releases the waits.
        ctx.mark_database_runtime_initializing_for_test();
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let route = route_key(42, 1);
        let routes = HashMap::from([(route, test_support::route_identity(&root, "test"))]);
        let frame = |name: &str, corr| DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                42,
                1,
                corr,
                serde_json::to_vec(&json!({"name": name, "arguments": {}})).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = DatabaseWaits::default();
        assert!(waits.defer(frame("bash", 1), &routes, &executor).is_ok());
        for name in [
            "read",
            "grep",
            "glob",
            "outline",
            "zoom",
            "aft_search",
            "aft_inspect",
        ] {
            assert!(
                waits.defer(frame(name, 2), &routes, &executor).is_err(),
                "{name} waited for DB"
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), waits.next())
                .await
                .is_err()
        );
        assert!(waits.cancel(route, 1).is_some());
        assert!(waits.pending.is_empty());
        assert!(waits.defer(frame("bash", 3), &routes, &executor).is_ok());
        ctx.finish_database_runtime(Ok(()));
        let resumed = tokio::time::timeout(Duration::from_millis(500), waits.next())
            .await
            .unwrap();
        assert_eq!(
            resumed.frame.header.corr, 3,
            "cancelled call must never resume"
        );
        assert!(waits.pending.is_empty());
    }
}
