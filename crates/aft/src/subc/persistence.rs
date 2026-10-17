//! Persistence readiness waits must not occupy the frame loop or an executor worker.

use super::*;

const WAIT_BUDGET: Duration = Duration::from_secs(10);
const MAX_WAITERS: usize = 256;

#[derive(Default)]
pub(super) struct DatabaseWaits {
    tasks: tokio::task::JoinSet<DecodedFrame>,
    pending: HashMap<(RouteChannel, u64), (tokio::task::AbortHandle, AdmissionWait)>,
}

impl DatabaseWaits {
    pub(super) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Return the frame unchanged unless this is a persistence-dependent call
    /// on a bound root whose database is opening, or any catalog fetch or
    /// tool call on a scoped worker route whose saved confinement is not yet
    /// known. Failure and overflow are answered by the ordinary tool refusal
    /// path without waiting.
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
            if confine_wait && matches!(operation, "tool.catalog" | "tool.call") {
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
    /// not admitted before then. Returns the frame unchanged when no read is
    /// needed. Past the waiter limit the frame also comes back unchanged, and
    /// admission refuses it for want of a policy (see
    /// `remote_policy::admit_confined_on_frame_loop`).
    pub(super) fn read_confinement(
        &mut self,
        decoded: DecodedFrame,
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
        if remote_policy::confinement_from_memory(&ctx, identity).is_some() {
            return Err(decoded);
        }
        let Ok(body) = decoded.body() else {
            return Err(decoded);
        };
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
        let name = body
            .get("name")
            .or_else(|| body.get("op"))
            .and_then(Value::as_str)
            .unwrap_or("tool.call")
            .to_owned();
        if self.pending.len() >= MAX_WAITERS {
            return Err(decoded);
        }
        let key = (route, decoded.frame.header.corr);
        let admission = AdmissionWait::from_decoded(&decoded, &name);
        let identity = identity.clone();
        let mut decoded = decoded;
        let handle = self.tasks.spawn(async move {
            let read =
                tokio::task::spawn_blocking(move || remote_policy::confined(&ctx, &identity))
                    .await
                    .unwrap_or_else(|_| Err(remote_policy::policy_unavailable()));
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
        let ctx = Arc::clone(ctx);
        let handle = self.tasks.spawn(async move {
            if any_command {
                ctx.wait_for_database_open(deadline).await;
            } else {
                ctx.wait_for_database_runtime(&name, deadline).await;
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
    async fn stale_database_completion_cannot_remove_replacement_waiter() {
        let route = route_key(42, 1);
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
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
            disabled_tools: Arc::default(),
            scope: Some(scope),
            call_mode: subc_protocol::call_mode::CallMode::Classic,
            run_scope: OnceLock::new(),
            made_tool_call: AtomicBool::new(false),
        }));
        assert!(remote_policy::key(&worker, Some("worker")).is_some());
        (worker, unscoped)
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
        assert_eq!(resumed.confinement, Some(Ok(true)));
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
        assert_eq!(resumed.confinement, Some(Ok(false)));
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
        assert!(second.confinement.is_none());
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
