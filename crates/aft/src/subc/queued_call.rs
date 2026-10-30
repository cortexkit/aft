//! Admission deadlines apply only before a command starts, never to its effects.

use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct Deadline {
    at: Instant,
    received_at: Instant,
    unix_ms: u64,
}

impl Deadline {
    pub(super) fn from_envelope(received_at: Instant, body: &Value) -> Option<Self> {
        let unix_ms = body.get("deadline_ms")?.as_u64()?;
        let now = Instant::now();
        let wall = crate::bash_background::persistence::unix_millis();
        Some(Self {
            at: now
                .checked_add(Duration::from_millis(unix_ms.saturating_sub(wall)))
                .unwrap_or(now),
            received_at,
            unix_ms,
        })
    }

    pub(super) fn at(self) -> Instant {
        self.at
    }

    fn expired(self) -> bool {
        Instant::now() >= self.at
    }

    pub(super) fn log_drop(self, tool: &str, reason: &str) {
        log::warn!(
            "queued tool_call dropped name={tool:?} reason={reason} queue_ms={} deadline_ms={}",
            self.received_at.elapsed().as_millis(),
            self.unix_ms,
        );
    }
}

pub(super) fn guard(
    deadline: Option<Deadline>,
    tool: &str,
    request_id: String,
    job: crate::executor::ExecutorJob,
) -> crate::executor::ExecutorJob {
    let tool = tool.to_owned();
    Box::new(move |ctx| {
        if let Some(deadline) = deadline.filter(|deadline| deadline.expired()) {
            deadline.log_drop(&tool, "client_deadline");
            return Response::error(
                request_id,
                "deadline_exceeded",
                "client deadline passed before execution; the tool did not run",
            );
        }
        job(ctx)
    })
}

pub(super) fn release_route(
    active: &ActiveToolCalls,
    executor: &Executor,
    route: RouteChannel,
    deferred: &PendingSubcResponses,
) -> Vec<(u64, RequestFrameMeta)> {
    // The pending-response path sends deferred calls' final responses, even if
    // their continuation's cancellation token still says pending. Leave those
    // calls to that path so route release cannot send a second terminal.
    let deferred: HashSet<_> = deferred
        .entries
        .iter()
        .filter(|entry| entry.route == route)
        .map(|entry| entry.corr)
        .collect();
    let mut calls = active.lock().unwrap_or_else(|e| e.into_inner());
    let mut terminals = Vec::new();
    calls.retain(|(call_route, corr), call| {
        if *call_route != route || call.answering || deferred.contains(corr) {
            return true;
        }
        if executor.cancel_job_before_execution(&call.root_id, &call.cancellation)
            == PreExecutionCancelOutcome::AlreadyStarted
        {
            return true;
        }
        if let Some(deadline) = call.request.deadline {
            deadline.log_drop(&call.tool, "route_released");
        } else {
            log::warn!(
                "queued tool_call dropped name={:?} reason=route_released queue_ms={} deadline_ms=none",
                call.tool, call.started_at.elapsed().as_millis(),
            );
        }
        terminals.push((*corr, call.request));
        false
    });
    terminals
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    static EXECUTED: AtomicBool = AtomicBool::new(false);

    fn dispatch(request: RawRequest, _: &AppContext) -> Response {
        EXECUTED.store(true, Ordering::SeqCst);
        Response::success(request.id, json!({"text": "ran"}))
    }

    #[tokio::test]
    async fn queued_call_deadline_skips_execution_and_sends_one_terminal_but_live_call_runs() {
        for expired in [true, false] {
            EXECUTED.store(false, Ordering::SeqCst);
            let (dir, root) = test_support::test_root("queued-deadline");
            let file = dir.path().join("source.txt");
            std::fs::write(&file, "hello").unwrap();
            let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
                pool_size: 2,
                actor_cap: 1,
                read_cap: 1,
                ..Default::default()
            }));
            executor.register_actor(root.clone(), test_support::test_ctx());
            let (started, running) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let blocker = executor.submit_async(
                root.clone(),
                Lane::Mutating,
                "blocker".into(),
                Box::new(move |_| {
                    let _ = started.send(());
                    let _ = held.recv();
                    Response::success("blocker", json!({}))
                }),
            );
            running.await.unwrap();
            let deadline_ms = crate::bash_background::persistence::unix_millis()
                + if expired { 100 } else { 60_000 };
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                41,
                1,
                7,
                serde_json::to_vec(
                    &json!({"name":"read", "deadline_ms":deadline_ms, "arguments":{"path":file}}),
                )
                .unwrap(),
            )
            .unwrap();
            let (writer, mut replies) = mpsc::channel(8);
            let (bash_tx, _bash_rx) = mpsc::channel(8);
            let (touch_tx, _touch_rx) = mpsc::channel(8);
            let (entries, _deferred_rx) = mpsc::unbounded_channel();
            let deferred = DeferredResponseSender {
                entries,
                wake: Default::default(),
            };
            let active = Arc::default();
            handle_tool_call(
                &writer,
                &frame,
                None,
                None,
                PhaseTrace::new(Instant::now()),
                &HashMap::from([(
                    route_key(41, 1),
                    test_support::route_identity(&root, "deadline-test"),
                )]),
                &HashMap::new(),
                &ReclaimedRoutes::default(),
                &mut HashMap::new(),
                &executor,
                &active,
                &Arc::new(PendingDeferredSetups::default()),
                &Arc::new(Notify::new()),
                &PersistentCancelSignal::new(),
                &bash_tx,
                &touch_tx,
                &Arc::new(DispatchPathMetrics::new()),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut 1,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                dispatch,
                &deferred,
                false,
                1024 * 1024,
                &drain::ModuleDrainWindow::default(),
                None,
            )
            .await
            .unwrap();
            assert!(
                !active.lock().unwrap().is_empty(),
                "call must really be queued"
            );
            if expired {
                tokio::time::sleep(Duration::from_millis(
                    deadline_ms.saturating_sub(crate::bash_background::persistence::unix_millis())
                        + 1,
                ))
                .await;
            }
            release.send(()).unwrap();
            blocker.await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(2), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply.header.ty, FrameType::Response);
            assert_eq!(reply.header.corr, 7);
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(EXECUTED.load(Ordering::SeqCst), !expired, "{body}");
            assert_eq!(body["structuredContent"]["success"], !expired, "{body}");
            if expired {
                assert_eq!(body["structuredContent"]["code"], "deadline_exceeded");
            }
            tokio::task::yield_now().await;
            assert!(active.lock().unwrap().is_empty());
            assert!(
                replies.try_recv().is_err(),
                "each request owes exactly one terminal"
            );
        }
    }

    #[tokio::test]
    async fn released_route_discards_queued_mutation_with_terminal_and_never_cancels_started_work()
    {
        let (_dir, root) = test_support::test_root("released-queue");
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            actor_cap: 1,
            ..Default::default()
        }));
        executor.register_actor(root.clone(), test_support::test_ctx());
        let (started, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let blocker = executor.submit_async(
            root.clone(),
            Lane::Mutating,
            "blocker".into(),
            Box::new(move |_| {
                let _ = started.send(());
                let _ = held.recv();
                Response::success("blocker", json!({}))
            }),
        );
        running.await.unwrap();
        let route = route_key(41, 1);
        let active = Arc::default();
        let executed = Arc::new(AtomicBool::new(false));
        let probe = executed.clone();
        let queued = submit_active_tool_call(
            &executor,
            &active,
            route,
            7,
            root.clone(),
            Lane::Mutating,
            "queued-write".into(),
            RouteDetachPolicy::RetainForReplay,
            "write",
            test_support::test_request_meta(),
            Box::new(move |_| {
                probe.store(true, Ordering::SeqCst);
                Response::success("write", json!({}))
            }),
        );
        let (writer, mut replies) = mpsc::channel(8);
        let mut routes =
            HashMap::from([(route, test_support::route_identity(&root, "release-test"))]);
        teardown_installed_route(
            &writer,
            &DispatchPathMetrics::new(),
            &executor,
            route,
            "Goodbye",
            None,
            &mut HashMap::from([(41, 1)]),
            &mut routes,
            &mut HashMap::new(),
            &mut HashMap::from([(root.clone(), HashSet::from([route]))]),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &active,
            &mut PendingSubcResponses::default(),
            &mut HashMap::new(),
            &mut RetryBuffer::new(),
            &mut HashMap::new(),
            &Arc::new(Notify::new()),
            1024 * 1024,
            None,
        )
        .await
        .unwrap();
        let terminal = replies.recv().await.unwrap();
        assert_eq!(terminal.header.ty, FrameType::Error);
        assert_eq!(terminal.header.corr, 7);
        assert!(!queued.await.unwrap().success);
        release.send(()).unwrap();
        blocker.await.unwrap();
        assert!(!executed.load(Ordering::SeqCst));
        assert!(replies.try_recv().is_err());

        let (started, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let mutation = submit_active_tool_call(
            &executor,
            &active,
            route,
            8,
            root,
            Lane::Mutating,
            "started-write".into(),
            RouteDetachPolicy::RetainForReplay,
            "write",
            test_support::test_request_meta(),
            Box::new(move |_| {
                let _ = started.send(());
                let _ = held.recv();
                assert!(!crate::executor::current_job_cancelled());
                Response::success("write", json!({}))
            }),
        );
        running.await.unwrap();
        assert!(
            release_route(&active, &executor, route, &PendingSubcResponses::default()).is_empty()
        );
        release.send(()).unwrap();
        assert!(mutation.await.unwrap().success);
    }
}
