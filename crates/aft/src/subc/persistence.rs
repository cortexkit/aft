//! Persistence readiness waits must not occupy the frame loop or an executor worker.

use super::*;

/// How long a call may wait for its root's persistence before it is admitted
/// anyway and answered by its ordinary refusal path.
pub(super) const WAIT_BUDGET: Duration = Duration::from_secs(10);
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
    /// on a bound root whose database is opening. Failure and overflow are
    /// answered by the ordinary tool refusal path without waiting.
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
        if body
            .get("op")
            .and_then(Value::as_str)
            .is_some_and(tool_provider::recognized_operation)
            || body
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(tool_provider::recognized_operation)
        {
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
        if !ctx.database_runtime_pending(name) {
            return Err(decoded);
        }
        // A root reported as initializing always has its open staged or
        // running. Hand a staged open to the database-open thread now rather
        // than trusting that the bind's dispatch or the configure tail will
        // get to it soon: the tail can be queued behind other roots for
        // minutes, and a missed dispatch must not leave this root refusing
        // mutations indefinitely. A no-op when the open is already running.
        crate::database_open::dispatch(&ctx);
        if self.pending.len() >= MAX_WAITERS {
            return Err(decoded);
        }
        let name = name.to_string();
        let key = (route, decoded.frame.header.corr);
        let admission = AdmissionWait::from_decoded(&decoded, &name);
        // A caller may shorten the wait with an absolute Unix-millisecond
        // deadline. The bash timeout limits command execution, not the wait
        // for permission to admit the command.
        let mut deadline = decoded.phase_trace.received_at() + WAIT_BUDGET;
        if let Some(epoch_ms) = body.get("deadline_ms").and_then(Value::as_u64) {
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
        let handle = self.tasks.spawn(async move {
            ctx.wait_for_database_runtime(&name, deadline).await;
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
