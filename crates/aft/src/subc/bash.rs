//! Deferred bash orchestration and trust-gated shell helpers for subc route calls.

use super::*;

#[derive(Clone)]
pub(super) struct BashWaitCancel {
    pub(super) speech_seed: Option<crate::gh_shim_speech::service::LaunchSeed>,
    pub(super) run_scope: Option<Arc<super::run_scope::RunScope>>,
    pub(super) scope: Option<subc_protocol::scope::ScopeStamp>,
    pub(super) late_results: bool,
    /// The call-ledger carrier to record this call under when it is not the
    /// spawn principal's own id: a codemode program's call runs with its head
    /// agent's principal, but its ledger rows belong to the run route that sent
    /// it, so `late_results` and `tool.withdraw` on that route find them.
    pub(super) ledger_carrier: Option<String>,
    pub(super) connection: PersistentCancelSignal,
    pub(super) route: PersistentCancelSignal,
    /// Open while the daemon drains this module; a foreground wait that sees it
    /// detaches its command into a background task and answers at once.
    pub(super) drain: drain::ModuleDrainWindow,
}

impl BashWaitCancel {
    async fn cancelled(&self) {
        tokio::select! {
            _ = self.connection.cancelled() => {}
            _ = self.route.cancelled() => {}
        }
    }
}

pub(super) struct RouteBashCancel {
    pub(super) token: PersistentCancelSignal,
    pub(super) active_waits: usize,
}

pub(super) struct BashElicitationPlan {
    pub(super) command: String,
    pub(super) asks: Vec<crate::bash_permissions::PermissionAsk>,
    pub(super) grants: Vec<String>,
}

pub(super) struct BashDeferredCompletion {
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: Option<ToolCallResult>,
    fatal: bool,
    /// True for the completion that removes the held request and decrements its
    /// root and route wait counts. A panic observed after a deadline reply sets
    /// this false because that reply already settled those counts.
    settles_hold: bool,
}

#[cfg(test)]
impl BashDeferredCompletion {
    pub(super) fn response_for_test(&self) -> &Response {
        &self
            .result
            .as_ref()
            .expect("bash completion result")
            .response
    }

    /// The text the caller sees, or `None` for a completion that carries no
    /// result (the call was already answered elsewhere, or cancelled).
    pub(super) fn text_for_test(&self) -> Option<&str> {
        self.result.as_ref().map(|result| result.text.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BashTranslatedSettings {
    background: bool,
    pty: bool,
    wait: bool,
    block_to_completion: bool,
    timeout: Option<u64>,
}

enum BashSpawnControl {
    Immediate,
    Foreground {
        task_id: String,
        session_id: String,
        project_root: Option<PathBuf>,
        storage_dir: PathBuf,
        deadline: Instant,
        block_to_completion: bool,
        /// Held-call diagnostics retain the requested blocking mode even if
        /// a deadline returns a still-running task to the caller.
        blocking_intent: bool,
        timeout: Option<u64>,
        wait_window_ms: u64,
        detach_on_user_message: bool,
        worker_session: bool,
        /// The configured wait limit bounding a blocking call in any session
        /// (`wait: true` or `block_to_completion`).
        worker_cap_ms: Option<u64>,
        /// Cached before polling so the reply watchdog never needs task locks.
        blocking_handoff_response: Option<Response>,
        /// For a blocking call: the instant its wait hands a remote task back
        /// to the background, and that wait in milliseconds from when the
        /// call arrived (see `remote_block_handback_ms`).
        remote_handback: Option<(Instant, u64)>,
    },
}

enum BashPollControl {
    Done,
    Promote,
    Wait,
    /// The module loop already answered the call; the poll changed nothing.
    Abandoned,
}

/// Ends a poll job that found its call already answered by the module loop,
/// without touching the task or its wait registration (the loop owns both).
fn abandon_bash_poll(
    request_id: String,
    control_tx: &mut Option<oneshot::Sender<BashPollControl>>,
) -> Response {
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashPollControl::Abandoned);
    }
    Response::success(request_id, json!({ "subc_bash_step": "abandoned" }))
}

/// Ends the wait registration a foreground bash call holds for its task.
fn release_wait_registration(
    registry: &crate::bash_background::BgTaskRegistry,
    session_id: &str,
    task_id: &str,
    wait_mode: bool,
) {
    if wait_mode {
        registry.end_wait_mode_session(session_id, task_id);
    } else {
        registry.unregister_foreground_task(session_id, task_id);
    }
}

/// What one wake of a deferred bash wait learned about its task.
struct DeferredWaitObservation {
    /// The task reached a terminal state, or is no longer in the registry.
    target_finished: bool,
    /// A `wait: true` call's session asked to detach the wait.
    detach_pending: bool,
    remote_handback_due: bool,
}

/// Reads a deferred bash wait's task state on the blocking pool.
///
/// The wait loop runs on the subc frame loop's single-threaded runtime. The
/// registry poll takes the task's state mutex, which other threads can hold
/// while persisting JSON or spawning a local fallback. Doing that inline stops
/// the frame loop, so no route's frames are read or written until the lock frees.
/// Here the frame loop only awaits, including when classifying a remote task.
///
/// For a delegated worker's capped blocking wait, `renew_window` keeps the
/// task's default hard kill at least that far away. Primary sessions also have
/// a wait cap, but retain their original hard-kill limit.
async fn observe_deferred_bash_wait(
    registry: crate::bash_background::BgTaskRegistry,
    task_id: String,
    session_id: String,
    wait_mode: bool,
    worker_session: bool,
    renew_window: Option<Duration>,
    remote_handback_at: Option<Instant>,
) -> DeferredWaitObservation {
    tokio::task::spawn_blocking(move || {
        if let Some(window) = renew_window.filter(|_| worker_session) {
            registry.renew_hard_kill(&task_id, &session_id, window);
        }
        // Only the finished/running answer matters here; the executor poll
        // that answers the call builds the full snapshot once.
        let target_finished = registry.wait_target_finished(&task_id, &session_id);
        let detach_pending = wait_mode && registry.wait_mode_detach_pending(&session_id);
        let remote_handback_due = remote_handback_at.is_some_and(|at| Instant::now() >= at)
            && registry.is_remote_task(&task_id, &session_id);
        DeferredWaitObservation {
            target_finished,
            detach_pending,
            remote_handback_due,
        }
    })
    .await
    // A panicked read learned nothing; hand the decision to the executor poll,
    // which reads the task again and answers the call either way.
    .unwrap_or(DeferredWaitObservation {
        target_finished: true,
        detach_pending: false,
        remote_handback_due: false,
    })
}

#[cfg(test)]
mod progress_lifetime_tests {
    use super::*;

    #[test]
    fn progress_record_is_cleared_when_held_call_is_removed_or_abandoned() {
        let context = super::super::test_support::test_ctx();
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        for removed in [true, false] {
            let calls = drain::HeldBashCalls::default();
            calls.insert(route, 1);
            calls.record_progress(route, 1, &context, "progress-lifetime");
            assert!(context.request_handles_progress("progress-lifetime"));
            if removed {
                calls.remove(route, 1);
            }
            drop(calls);
            assert!(!context.request_handles_progress("progress-lifetime"));
        }
    }

    #[test]
    fn cancelling_before_a_detach_target_clears_progress_but_keeps_the_hold() {
        let context = super::super::test_support::test_ctx();
        let calls = drain::HeldBashCalls::default();
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        calls.insert(route, 1);
        calls.record_progress(route, 1, &context, "cancelled-progress");
        assert!(context.request_handles_progress("cancelled-progress"));
        assert!(calls.request_cancel(route, 1));
        assert!(!context.request_handles_progress("cancelled-progress"));
        assert_eq!(calls.phase(route, 1), Some(drain::BashHoldPhase::Spawning));
    }

    #[test]
    fn retiring_a_route_prevents_late_cleanup_from_erasing_reused_id_progress() {
        let context = super::super::test_support::test_ctx();
        let calls = drain::HeldBashCalls::default();
        let old_route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        let new_route = RouteChannel {
            channel: 1,
            epoch: 2,
        };
        calls.insert(old_route, 1);
        calls.record_progress(old_route, 1, &context, "subc-1-1");
        calls.clear_progress_for_route(old_route);
        assert!(!context.request_handles_progress("subc-1-1"));
        calls.insert(new_route, 1);
        calls.record_progress(new_route, 1, &context, "subc-1-1");
        calls.remove(old_route, 1);
        assert!(context.request_handles_progress("subc-1-1"));
        calls.remove(new_route, 1);
        assert!(!context.request_handles_progress("subc-1-1"));
    }

    #[test]
    fn connection_cleanup_clears_progress_while_response_tasks_retain_metrics() {
        let context = super::super::test_support::test_ctx();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let lifetime = drain::ProgressLifetimeGuard::new(Arc::clone(&metrics));
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        metrics.held_bash_calls.insert(route, 1);
        metrics
            .held_bash_calls
            .record_progress(route, 1, &context, "connection-progress");
        assert!(context.request_handles_progress("connection-progress"));
        drop(lifetime);
        assert!(!context.request_handles_progress("connection-progress"));
        assert_eq!(
            metrics.held_bash_calls.phase(route, 1),
            Some(drain::BashHoldPhase::Spawning)
        );
    }
}

#[cfg(all(test, unix))]
mod frame_loop_lock_tests {
    use super::*;

    #[tokio::test]
    async fn module_drain_keeps_live_capture_when_stale_replay_loses_registration() {
        let (dir, registry, task) =
            crate::bash_background::registry::remote::captured_task_after_losing_replay_for_test();
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let cached = registry.drain_reply_cache(&task.task_id, &task.session_id);
        let metrics = DispatchPathMetrics::new();
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        metrics.held_bash_calls.insert(route, 1);
        metrics
            .held_bash_calls
            .set_phase(route, 1, drain::BashHoldPhase::Wait);
        metrics.held_bash_calls.set_detach_target(
            route,
            1,
            drain::BashDetachTarget {
                reply_cache: cached.clone(),
                run_scoped: true,
                task_id: task.task_id.clone(),
                session_id: task.session_id.clone(),
                wait_mode: false,
                worker_session: false,
                server_completion: false,
                late_results: false,
                registry: registry.clone(),
                request_id: "losing-replay".into(),
                ver: PROTOCOL_VERSION,
                flags: Flags::new(false, Priority::Passive, false),
                format_context: Default::default(),
            },
        );
        let routes = HashMap::from([(
            route,
            super::super::test_support::route_identity(&root, &task.session_id),
        )]);
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(4);
        let state = task.state.lock().unwrap();
        assert!(state.metadata.remote.as_ref().unwrap().accepted);
        let no_task_locks = crate::bash_background::registry::forbid_task_state_locks_for_test();
        let reply = answer_held_bash_calls_from_module_loop(&writer_tx, &routes, &metrics, true);
        tokio::pin!(reply);
        let answered = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(reply.as_mut(), cx))
        })
        .await;
        assert!(
            matches!(answered, std::task::Poll::Ready(Ok(1))),
            "drain must not wait on task state: {answered:?}"
        );
        let frame = writer_rx.try_recv().expect("drain frame").frame;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(
            body["structuredContent"]["status"], "running",
            "losing replay rewound capture: {body}"
        );
        assert!(
            !body.to_string().contains(REMOTE_INPUT_CAPTURE_WARNING),
            "{body}"
        );
        assert!(
            !body.to_string().contains("stale pre-capture replay"),
            "{body}"
        );
        let published = cached.snapshot();
        assert_eq!(published.inputs_captured, Some(true));
        assert_eq!(
            published.execution_note.as_deref(),
            Some("live input capture recorded")
        );
        assert_eq!(published.deadline.unwrap().0.limit_ms, 60_000);
        drop(no_task_locks);
        drop(state);
    }

    #[tokio::test]
    async fn module_drain_reports_uncaptured_terminal_before_output_rendering() {
        for refused in [true, false] {
            let fixture = crate::bash_background::registry::remote::uncaptured_terminal_before_render_for_test(refused);
            let root = ProjectRootId::from_path(fixture.dir.path()).unwrap();
            let task = &fixture.task;
            let cached = fixture
                .registry
                .drain_reply_cache(&task.task_id, &task.session_id);
            let published = cached.snapshot();
            assert_eq!(published.inputs_captured, Some(false));
            assert!(
                published
                    .uncaptured_terminal_facts
                    .as_ref()
                    .is_some_and(|reply| reply.data["status"] == "failed"),
                "finished facts must accompany terminal metadata"
            );
            assert!(
                published.uncaptured_terminal.is_none(),
                "the terminal renderer must still be gated"
            );
            let metrics = DispatchPathMetrics::new();
            let route = RouteChannel {
                channel: 1,
                epoch: 1,
            };
            metrics.held_bash_calls.insert(route, 1);
            metrics
                .held_bash_calls
                .set_phase(route, 1, drain::BashHoldPhase::Wait);
            metrics.held_bash_calls.set_detach_target(
                route,
                1,
                drain::BashDetachTarget {
                    reply_cache: cached,
                    run_scoped: true,
                    task_id: task.task_id.clone(),
                    session_id: task.session_id.clone(),
                    wait_mode: false,
                    worker_session: false,
                    server_completion: false,
                    late_results: false,
                    registry: fixture.registry.clone(),
                    request_id: "terminal-before-render".into(),
                    ver: PROTOCOL_VERSION,
                    flags: Flags::new(false, Priority::Passive, false),
                    format_context: Default::default(),
                },
            );
            let routes = HashMap::from([(
                route,
                super::super::test_support::route_identity(&root, &task.session_id),
            )]);
            let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(4);
            let state = task.state.lock().unwrap();
            assert!(state.metadata.status.is_terminal());
            let no_task_locks =
                crate::bash_background::registry::forbid_task_state_locks_for_test();
            let reply =
                answer_held_bash_calls_from_module_loop(&writer_tx, &routes, &metrics, true);
            tokio::pin!(reply);
            let answered = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(reply.as_mut(), cx))
            })
            .await;
            assert!(
                matches!(answered, std::task::Poll::Ready(Ok(1))),
                "drain must not wait for rendering: {answered:?}"
            );
            let frame = writer_rx.try_recv().expect("terminal drain frame").frame;
            let body: Value = serde_json::from_slice(&frame.body).unwrap();
            assert_eq!(
                body["structuredContent"]["status"], "failed",
                "refused={refused}: {body}"
            );
            assert_eq!(
                body["structuredContent"]["exit_code"],
                if refused { Value::Null } else { json!(137) }
            );
            assert!(
                !body.to_string().contains(REMOTE_INPUT_CAPTURE_WARNING),
                "{body}"
            );
            assert!(
                !body
                    .to_string()
                    .contains("Detached because AFT is restarting"),
                "{body}"
            );
            if refused {
                assert_eq!(body["structuredContent"]["success"], false, "{body}");
                assert_eq!(
                    body["structuredContent"]["code"], "remote_unavailable",
                    "{body}"
                );
                assert!(body.to_string().contains("unreachable"), "{body}");
            } else {
                assert!(body.to_string().contains("remote signal 9"), "{body}");
            }
            drop(no_task_locks);
            drop(state);
        }
    }

    #[tokio::test]
    async fn due_remote_handback_check_never_locks_task_state_on_frame_loop() {
        let (_dir, ctx, task) =
            crate::bash_background::registry::default_kill_tests::wait_fixture(None);
        let root = ProjectRootId::from_path(_dir.path()).unwrap();
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        executor.register_actor(root.clone(), Arc::new(ctx));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(4);
        let (poll_touch_tx, _poll_touch_rx) = mpsc::channel(4);
        let _no_task_locks = crate::bash_background::registry::forbid_task_state_locks_for_test();
        // A local task still has to be classified when the remote hand-back
        // deadline is due. That classification must stay on a worker thread.
        tokio::time::timeout(
            Duration::from_secs(60),
            run_deferred_bash_wait(
                executor,
                completion_tx,
                poll_touch_tx,
                metrics,
                RouteChannel {
                    channel: 1,
                    epoch: 1,
                },
                1,
                Flags::new(false, Priority::Passive, false),
                PROTOCOL_VERSION,
                root,
                "remote-handback-lock".into(),
                task.task_id.clone(),
                task.session_id.clone(),
                Some(_dir.path().into()),
                _dir.path().into(),
                Instant::now(),
                false,
                None,
                1,
                false,
                false,
                None,
                Some((Instant::now(), 0)),
                crate::subc_format::FormatContext::default(),
                BashWaitCancel {
                    speech_seed: None,
                    run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(),
                },
                Arc::new(drain::BashCallClaim::default()),
                None,
                false,
            ),
        )
        .await
        .expect("deferred wait hung");
        let completion = completion_rx.recv().await.unwrap();
        let response = completion.result.unwrap().response;
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["task_id"], task.task_id);
        assert_eq!(response.data["status"], "running");
    }

    #[tokio::test]
    async fn module_drain_reply_never_locks_task_state_on_frame_loop() {
        let (_dir, ctx, task) =
            crate::bash_background::registry::default_kill_tests::wait_fixture(None);
        let root = ProjectRootId::from_path(_dir.path()).unwrap();
        let metrics = DispatchPathMetrics::new();
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        let claim = metrics.held_bash_calls.insert(route, 1);
        metrics
            .held_bash_calls
            .set_phase(route, 1, drain::BashHoldPhase::Wait);
        metrics.held_bash_calls.set_detach_target(
            route,
            1,
            drain::BashDetachTarget {
                reply_cache: ctx
                    .bash_background()
                    .drain_reply_cache(&task.task_id, &task.session_id),
                run_scoped: true,
                task_id: task.task_id.clone(),
                session_id: task.session_id.clone(),
                wait_mode: false,
                worker_session: false,
                server_completion: false,
                late_results: false,
                registry: ctx.bash_background().clone(),
                request_id: "drain-lock".into(),
                ver: PROTOCOL_VERSION,
                flags: Flags::new(false, Priority::Passive, false),
                format_context: crate::subc_format::FormatContext::default(),
            },
        );
        let routes = HashMap::from([(
            route,
            super::super::test_support::route_identity(&root, &task.session_id),
        )]);
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(4);
        let registry = ctx.bash_background();
        let initial_note = crate::commands::bash_orchestrate::kill_deadline_note(
            registry,
            &task.task_id,
            &task.session_id,
            false,
        );
        // Renew after registering the detach target: drain must see the new
        // deadline even when the update did not need a durable metadata write.
        registry
            .renew_hard_kill(&task.task_id, &task.session_id, Duration::from_secs(3600))
            .unwrap();
        registry
            .renew_hard_kill(&task.task_id, &task.session_id, Duration::from_secs(3601))
            .unwrap();
        let expected = stateful_module_drain_reply_for_test(
            registry,
            &task.task_id,
            &task.session_id,
            "drain-lock",
            false,
            false,
        );
        assert_ne!(
            initial_note,
            crate::commands::bash_orchestrate::kill_deadline_note(
                registry,
                &task.task_id,
                &task.session_id,
                false
            )
        );
        let _state = task.state.lock().unwrap();
        let _no_task_locks = crate::bash_background::registry::forbid_task_state_locks_for_test();
        // The frame loop awaits this function directly. It must be ready even
        // while task state is unavailable, not merely offload a blocking read
        // and leave the module loop waiting for that worker.
        let reply = answer_held_bash_calls_from_module_loop(&writer_tx, &routes, &metrics, true);
        tokio::pin!(reply);
        let answered = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(reply.as_mut(), cx))
        })
        .await;
        assert!(
            matches!(answered, std::task::Poll::Ready(Ok(1))),
            "drain reply waited on task state: {answered:?}"
        );
        let frame = writer_rx.recv().await.unwrap().frame;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["structuredContent"]["task_id"], task.task_id);
        assert_eq!(body["structuredContent"]["status"], "running");
        assert_eq!(
            body["structuredContent"]["output"], expected.data["output"],
            "drain must preserve the state-reading reply byte for byte"
        );
        assert_eq!(
            body["content"][0]["text"],
            bash_result_from_response(expected, &Default::default()).text
        );
        assert!(claim.claimed_by_module_loop());
        assert!(writer_rx.try_recv().is_err(), "exactly one reply");
    }
}

#[cfg(all(test, unix))]
fn stateful_module_drain_reply_for_test(
    registry: &crate::bash_background::BgTaskRegistry,
    task_id: &str,
    session_id: &str,
    request_id: &str,
    worker_session: bool,
    bash_watch_available: bool,
) -> Response {
    let response = Response::success(
        request_id,
        json!({
            "output": format!("{}{}",
                crate::commands::bash_orchestrate::format_module_drain_detach_message(task_id, worker_session, bash_watch_available),
                crate::commands::bash_orchestrate::kill_deadline_note(registry, task_id, session_id, worker_session)),
            "task_id": task_id,
            "status": "running",
        }),
    );
    crate::commands::bash_orchestrate::with_runon_capture_state(
        registry, task_id, session_id, response,
    )
}

#[cfg(test)]
mod default_kill_tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn a02_observation_polls_the_live_child_once() {
        use std::sync::atomic::Ordering;
        let (_dir, ctx, task) =
            crate::bash_background::registry::default_kill_tests::wait_fixture(None);
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        task.state.lock().unwrap().runtime =
            crate::bash_background::registry::TaskRuntime::Piped(Some(child.into()));
        let registry = ctx.bash_background();
        let observation = observe_deferred_bash_wait(
            registry.clone(),
            task.task_id.clone(),
            task.session_id.clone(),
            false,
            false,
            None,
            None,
        )
        .await;
        let polls = registry.inner.child_polls.load(Ordering::Relaxed);
        if let crate::bash_background::registry::TaskRuntime::Piped(Some(mut child)) =
            std::mem::replace(
                &mut task.state.lock().unwrap().runtime,
                crate::bash_background::registry::TaskRuntime::Piped(None),
            )
        {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(!observation.target_finished);
        assert_eq!(polls, 1, "each observation must poll the actual child");
    }

    /// A held call observes its task once per poll interval (and on every
    /// terminal transition in the registry). Those observations must not
    /// build task snapshots, which open task files on disk: with many held
    /// calls, per-observation snapshots kept the blocking pool busy.
    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_wait_observations_build_no_snapshots() {
        use std::sync::atomic::Ordering;
        const OBSERVATIONS: u64 = 50;
        let (_dir, ctx, task) =
            crate::bash_background::registry::default_kill_tests::wait_fixture(None);
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        task.state.lock().unwrap().runtime =
            crate::bash_background::registry::TaskRuntime::Piped(Some(child.into()));
        let registry = ctx.bash_background();
        let snapshots_before = registry.inner.snapshots_built.load(Ordering::Relaxed);
        let mut finished = 0;
        for _ in 0..OBSERVATIONS {
            let observation = observe_deferred_bash_wait(
                registry.clone(),
                task.task_id.clone(),
                task.session_id.clone(),
                false,
                false,
                None,
                None,
            )
            .await;
            finished += u64::from(observation.target_finished);
        }
        let polls = registry.inner.child_polls.load(Ordering::Relaxed);
        let snapshots = registry.inner.snapshots_built.load(Ordering::Relaxed) - snapshots_before;
        // End the child while it is still running, then check that a finished
        // task is reported finished without a snapshot either.
        if let crate::bash_background::registry::TaskRuntime::Piped(Some(mut child)) =
            std::mem::replace(
                &mut task.state.lock().unwrap().runtime,
                crate::bash_background::registry::TaskRuntime::Piped(None),
            )
        {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        task.state.lock().unwrap().metadata.status =
            crate::bash_background::BgTaskStatus::Completed;
        let terminal = observe_deferred_bash_wait(
            registry.clone(),
            task.task_id.clone(),
            task.session_id.clone(),
            false,
            false,
            None,
            None,
        )
        .await;
        let missing = observe_deferred_bash_wait(
            registry.clone(),
            "bash-00000000000000ff".into(),
            task.session_id.clone(),
            false,
            false,
            None,
            None,
        )
        .await;
        let snapshots_total =
            registry.inner.snapshots_built.load(Ordering::Relaxed) - snapshots_before;
        eprintln!(
            "deferred wait observations={OBSERVATIONS} child_polls={polls} snapshots={snapshots_total}"
        );
        assert_eq!(finished, 0, "a running task was reported finished");
        assert_eq!(polls, OBSERVATIONS, "each observation must poll the child");
        assert_eq!(snapshots, 0, "running-task observations built snapshots");
        assert!(terminal.target_finished);
        assert!(missing.target_finished, "an unknown task ends the wait");
        assert_eq!(snapshots_total, 0, "observations built snapshots");
    }

    async fn deferred_wait_limit(worker_session: bool, explicit_timeout: Option<u64>) -> u64 {
        let (_dir, ctx, task) =
            crate::bash_background::registry::default_kill_tests::wait_fixture(explicit_timeout);
        let registry = ctx.bash_background();
        registry.begin_wait_mode_session(&task.session_id, &task.task_id);
        assert!(registry.signal_wait_mode_detach(&task.session_id));
        let observation = observe_deferred_bash_wait(
            registry.clone(),
            task.task_id.clone(),
            task.session_id.clone(),
            true,
            worker_session,
            Some(Duration::from_millis(1_800_000)),
            None,
        )
        .await;
        assert!(!observation.target_finished);
        assert!(observation.detach_pending);
        registry.end_wait_mode_session(&task.session_id, &task.task_id);
        registry
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .unwrap()
            .limit_ms
    }

    #[tokio::test]
    async fn head_deferred_wait_preserves_default_hard_kill() {
        assert_eq!(deferred_wait_limit(false, None).await, 1_800_000);
    }

    #[tokio::test]
    async fn worker_deferred_wait_renews_default_hard_kill() {
        assert!(deferred_wait_limit(true, None).await >= 1_801_900);
    }

    #[tokio::test]
    async fn worker_deferred_wait_preserves_explicit_hard_kill() {
        assert_eq!(deferred_wait_limit(true, Some(45_000)).await, 45_000);
    }
}

/// Hands a held call's command to the background the way a drain detach does
/// (the task keeps running and delivers its completion later), off the
/// executor and off the module loop's thread: promotion writes task metadata.
pub(super) fn detach_held_bash_in_background(target: drain::BashDetachTarget, cancelled: bool) {
    tokio::task::spawn_blocking(move || detach_held_bash_sync(target, cancelled));
}

fn build_detached_frame(
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
    flags: Flags,
    task_id: &str,
    late_results: bool,
) -> Result<Frame, SubcError> {
    Frame::build_with_version(
        ver,
        FrameType::Error,
        flags,
        channel,
        epoch,
        corr,
        serde_json::to_vec(
            &ErrorBody::new("detached", "call detached; command still running")
                .with_detail(json!({"task_id": task_id, "late_results": late_results})),
        )
        .map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)
}

fn detach_held_bash_sync(target: drain::BashDetachTarget, cancelled: bool) {
    if cancelled && target.server_completion {
        let _ = target.registry.kill(&target.task_id, &target.session_id);
    } else if target.run_scoped {
        // A run's carrier can reconnect without ending the run. Leave its
        // process alive until the daemon's scope.describe response confirms the run ended; the
        // command's normal hard timeout still applies while confirmation waits.
    } else if let Err(error) = target.registry.promote(&target.task_id, &target.session_id) {
        // Runs on a shared blocking-pool thread; tag with the task's session.
        crate::log_ctx::with_session(Some(target.session_id.clone()), || {
            crate::slog_warn!(
                "subc attach: bash background handoff failed task_id={:?} session={:?} error={:?}",
                target.task_id,
                target.session_id,
                error
            );
        });
    } else if cancelled {
        // The caller saw only the cancellation; name the still-running task
        // on the session's next bash call.
        target
            .registry
            .note_cancelled_call(&target.task_id, &target.session_id);
    }
    release_wait_registration(
        &target.registry,
        &target.session_id,
        &target.task_id,
        target.wait_mode,
    );
}

/// Answers the held bash calls the module loop must answer itself (see
/// [`drain::HeldBashCalls::claim_for_module_loop`]) and hands each command to
/// the background. A drained call gets the same "detached" response its own
/// wait loop would have sent; a cancelled call gets the `cancelled` error every
/// other cancelled tool call gets. Returns how many calls were answered.
pub(super) async fn answer_held_bash_calls_from_module_loop(
    tx: &WriterSender,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    metrics: &DispatchPathMetrics,
    drain_due: bool,
) -> Result<usize, SubcError> {
    let claimed = metrics.held_bash_calls.claim_for_module_loop(drain_due);
    let answered = claimed.len();
    for (route, corr, target, reason) in claimed {
        log::info!(
            "subc attach: answered held bash call {} on route {route} corr={corr} from the module loop (reason={reason:?}); task {} {}",
            target.request_id,
            target.task_id,
            if target.run_scoped { "remains governed by its run scope" } else { "keeps running in the background" }
        );
        let frame: Option<WriterFrame> = match (reason, routes.get(&route)) {
            (_, None) => None,
            (drain::BashLoopAnswer::Drain, Some(_)) if target.server_completion => Some(
                build_detached_frame(
                    target.ver,
                    route.channel,
                    route.epoch,
                    corr,
                    target.flags,
                    &target.task_id,
                    target.late_results,
                )?
                .into(),
            ),
            (drain::BashLoopAnswer::Drain, Some(identity)) => {
                let cached = target.reply_cache.snapshot();
                let response = Response::success(
                    &target.request_id,
                    json!({
                        "output": format!(
                        "{}{}",
                        crate::commands::bash_orchestrate::format_module_drain_detach_message(
                            &target.task_id,
                            target.worker_session,
                            target
                                .format_context
                                .bash_watch_available
                                .unwrap_or(target.worker_session),
                        ),
                        crate::commands::bash_orchestrate::kill_deadline_note_from_parts(
                            cached.execution_note.as_deref(), cached.deadline, target.worker_session,
                        ),
                    ),
                        "task_id": target.task_id,
                        "status": "running",
                    }),
                );
                // This function is awaited by the module loop itself, not by a
                // separate wait task. Even awaiting a blocking-pool read here
                // would stop it serving other routes. Metadata writers publish
                // the note and capture inputs before releasing task state;
                // only pure response formatting happens here.
                let response = if cached.inputs_captured == Some(false) {
                    if let Some(terminal) = cached
                        .uncaptured_terminal
                        .or(cached.uncaptured_terminal_facts)
                    {
                        let mut reply = copy_response(&terminal);
                        reply.id = target.request_id.clone();
                        reply
                    } else {
                        with_capture_warning(&response)
                    }
                } else {
                    response
                };
                let result = bash_result_from_response(response, &target.format_context);
                Some(build_tool_response_frame(
                    target.ver,
                    route,
                    corr,
                    target.flags,
                    &result,
                    identity.reply_policy(),
                )?)
            }
            (drain::BashLoopAnswer::Cancel, Some(_)) => Some(
                Frame::build_with_version(
                    target.ver,
                    FrameType::Error,
                    target.flags,
                    route.channel,
                    route.epoch,
                    corr,
                    serde_json::to_vec(
                        &ErrorBody::new("cancelled", "request cancelled")
                            .with_detail(json!({"task_id": target.task_id})),
                    )
                    .map_err(SubcError::Json)?,
                )
                .map_err(SubcError::FrameBuild)?
                .into(),
            ),
        };
        detach_held_bash_in_background(target, matches!(reason, drain::BashLoopAnswer::Cancel));
        if let Some(frame) = frame {
            send_reliable_writer_frame(tx, metrics, frame, "held bash answer").await?;
        }
    }
    Ok(answered)
}

fn bash_settings_from_translated(args: &serde_json::Map<String, Value>) -> BashTranslatedSettings {
    BashTranslatedSettings {
        background: args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        pty: args.get("pty").and_then(Value::as_bool).unwrap_or(false),
        wait: args.get("wait").and_then(Value::as_bool).unwrap_or(false),
        block_to_completion: args
            .get("block_to_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout: args.get("timeout").and_then(Value::as_u64),
    }
}

pub(super) fn prepare_bash_elicitation_plan(
    arguments: &Value,
    project_root: &Path,
) -> Result<BashElicitationPlan, crate::subc_translate::TranslateError> {
    let translated = crate::subc_translate::subc_translate("bash", arguments, project_root)?;
    let command = translated
        .args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let workdir = translated
        .args
        .get("workdir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| project_root.to_path_buf());
    // PowerShell is not POSIX shell. Its AST must never be fed to the bash
    // scanner, so untrusted routes require a conservative exact-command ask.
    let asks = if translated.args.get("shell").and_then(Value::as_str) == Some("powershell") {
        vec![crate::bash_permissions::PermissionAsk {
            kind: crate::bash_permissions::PermissionKind::Bash,
            patterns: vec![command.clone()],
            always: Vec::new(),
            reason: None,
            targets: Vec::new(),
        }]
    } else {
        crate::bash_permissions::scan::scan_with_project_root(&command, project_root, &workdir)
    };
    let grants = permission_grants_for_retry(&asks);
    Ok(BashElicitationPlan {
        command,
        asks,
        grants,
    })
}

fn permission_grants_for_retry(asks: &[crate::bash_permissions::PermissionAsk]) -> Vec<String> {
    asks.iter()
        .flat_map(|ask| {
            if ask.always.is_empty() {
                ask.patterns.iter()
            } else {
                ask.always.iter()
            }
        })
        .cloned()
        .collect()
}

fn finalized_bash_result(
    mut response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    allow_bg_completions: bool,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
) -> ToolCallResult {
    // A terminal task's stored view record rides `structuredContent` on
    // tool-result/v1 routes only; the bash text renderer reads `output`, so
    // the extra member leaves the model-visible content bytes unchanged.
    // Running replies and legacy routes are skipped inside the attach.
    ctx.bash_background()
        .attach_terminal_view(&mut response.data, session_id);
    // The bash formatter never renders `bg_completions`, so formatting before finalization
    // yields the same text and lets the finalizer append the status bar to it.
    let mut text =
        crate::subc_format::format_response_with_context("bash", &response, format_context);
    // Observe before finalizing: the status bar the finalizer appends carries
    // moving counts that would make identical results hash differently.
    if let Some(repeat) = repeat {
        repeat.observe(ctx, &mut text);
    }
    crate::response_finalize::finalize_tool_response(
        &mut response,
        &mut text,
        ctx,
        session_id,
        "bash",
        allow_bg_completions,
    );
    ToolCallResult { text, response }
}

fn bash_result_from_response(
    response: Response,
    format_context: &crate::subc_format::FormatContext,
) -> ToolCallResult {
    let mut text =
        crate::subc_format::format_response_with_context("bash", &response, format_context);
    if response.data["output"]
        .as_str()
        .is_some_and(|output| output.ends_with(REMOTE_INPUT_CAPTURE_WARNING))
        && !text.contains(REMOTE_INPUT_CAPTURE_WARNING)
    {
        text.push_str(&format!("\n{REMOTE_INPUT_CAPTURE_WARNING}"));
    }
    ToolCallResult { text, response }
}

fn observe_completed_bash_result(
    mut result: ToolCallResult,
    ctx: &AppContext,
    format_context: &crate::subc_format::FormatContext,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
) -> ToolCallResult {
    // Only the reply selected for delivery records the call in the repeat breaker.
    // Hash its bare command output, then restore the status bar and warning text
    // that the executor already appended, so changing counts do not change the hash.
    let mut text =
        crate::subc_format::format_response_with_context("bash", &result.response, format_context);
    let suffix = result
        .text
        .strip_prefix(&text)
        .unwrap_or_default()
        .to_owned();
    if let Some(repeat) = repeat {
        repeat.observe(ctx, &mut text);
    }
    text.push_str(&suffix);
    result.text = text;
    result
}

fn bash_background_launch_response(
    request_id: &str,
    task_id: &str,
    is_pty: bool,
    worker_session: bool,
    bash_watch_available: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!(
                "{}{deadline_note}",
                crate::commands::bash_orchestrate::format_background_launch(
                    task_id,
                    is_pty,
                    worker_session,
                    bash_watch_available,
                )
            ),
            "task_id": task_id,
            "status": "running",
            "mode": if is_pty { "pty" } else { "pipes" },
        }),
    )
}

// Large worktrees and busy runners can take time to capture inputs. Bound the
// acknowledgement wait at one minute, charged from receipt against any shorter
// command timeout rather than appended to the caller's existing budget.
const REMOTE_INPUT_CAPTURE_WAIT: Duration = Duration::from_secs(60);
use crate::commands::bash_orchestrate::{with_capture_warning, REMOTE_INPUT_CAPTURE_WARNING};

async fn warn_if_runon_capture_missing(
    registry: crate::bash_background::BgTaskRegistry,
    task_id: String,
    session: String,
    response: Response,
) -> Response {
    let fallback = copy_response(&response);
    tokio::task::spawn_blocking(move || {
        crate::commands::bash_orchestrate::with_runon_capture_state(
            &registry, &task_id, &session, response,
        )
    })
    .await
    .unwrap_or(fallback)
}

fn copy_response(response: &Response) -> Response {
    Response {
        id: response.id.clone(),
        success: response.success,
        data: response.data.clone(),
    }
}

/// Chooses a launch reply from state observed after dispatch returns. An accepted
/// background task gets a fresh task-handle response instead of reusing an older
/// `starting` response; foreground calls keep their normal wait path.
fn settled_runon_launch_reply(
    registry: &crate::bash_background::BgTaskRegistry,
    task_id: &str,
    session: &str,
    response: &Response,
    background_runon: bool,
) -> Option<Response> {
    if registry.runon_inputs_captured(task_id, session) == Some(true) {
        if let Some(snapshot) = registry
            .observed_status(
                task_id,
                session,
                crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .filter(|snapshot| snapshot.remote_refusal.is_some())
        {
            let now = Instant::now();
            return match crate::commands::bash_orchestrate::decide_bash_step(
                snapshot,
                now,
                false,
                false,
                now,
                &response.id,
            ) {
                crate::commands::bash_orchestrate::BashStep::Done(reply) => Some(reply),
                _ => None,
            };
        }
        return background_runon.then(|| {
            accepted_runon_launch_reply(registry.remote_progress(task_id, session), response)
        });
    }
    // Without Accepted, a task that already ended (refused, failed, outcome
    // unknown, killed) reports that saved outcome; only a live task says
    // `starting`.
    Some(crate::commands::bash_orchestrate::runon_uncaptured_reply(
        registry, task_id, session, response,
    ))
}

fn accepted_runon_launch_reply(
    progress: Option<crate::bash_background::registry::remote::RemoteProgress>,
    response: &Response,
) -> Response {
    let mut reply = copy_response(response);
    reply.data["status"] = json!("running");
    if let Some(progress) = progress {
        let phase = progress.phase.tag();
        reply.data["phase"] = json!(phase);
        reply.data["remote_phase"] = json!(phase);
        reply.data["remote_job_id"] = progress
            .job_id
            .map(|job_id| json!(job_id.to_string()))
            .unwrap_or(Value::Null);
    }
    let output_field =
        if reply.data.get("output").is_none() && reply.data.get("output_preview").is_some() {
            "output_preview"
        } else {
            "output"
        };
    if let Some(output) = reply.data[output_field].as_str() {
        let mut output = output.to_owned();
        for warning in [
            crate::commands::bash_orchestrate::REMOTE_INPUT_CAPTURE_WARNING,
            REMOTE_ADMISSION_PENDING_NOTE,
        ] {
            if let Some(prefix) = output.strip_suffix(warning) {
                output = prefix.trim_end_matches('\n').to_owned();
            }
        }
        reply.data[output_field] = json!(output);
    }
    reply
}

/// A deadline cannot wait for the blocking pool or task/artifact locks. Remote
/// commits publish this small in-memory projection before releasing task state,
/// so a delayed read never turns a saved Accepted or refusal into `starting`.
fn cached_runon_launch_reply(
    registry: &crate::bash_background::BgTaskRegistry,
    task_id: &str,
    session: &str,
    response: &Response,
    background_runon: bool,
) -> Option<Response> {
    if let Some(launch) = registry.runon_launch_state(task_id, session) {
        if let Some(terminal) = launch.terminal {
            let now = Instant::now();
            if let crate::commands::bash_orchestrate::BashStep::Done(reply) =
                crate::commands::bash_orchestrate::decide_bash_step(
                    terminal,
                    now,
                    false,
                    false,
                    now,
                    &response.id,
                )
            {
                return Some(reply);
            }
        }
        if launch.accepted {
            return background_runon
                .then(|| accepted_runon_launch_reply(Some(launch.progress), response));
        }
    }
    Some(with_capture_warning(response))
}

/// The launch reply when a deadline or module drain ends the capture wait.
/// Read the published launch state without waiting for task locks or the
/// blocking pool: accepted foreground calls keep their normal wait path,
/// accepted background calls get a refreshed task handle, and saved refusals
/// remain errors.
async fn unsettled_runon_launch_reply(
    registry: crate::bash_background::BgTaskRegistry,
    task_id: String,
    session: String,
    response: &Response,
    background_runon: bool,
) -> Option<Response> {
    cached_runon_launch_reply(&registry, &task_id, &session, response, background_runon)
}

/// The least time one capture-state read gets, even when the capture deadline
/// has already passed before the read starts (a slow dispatch can use up the
/// whole budget). Without it the first read after a late start always lost the
/// race against an expired deadline and answered with the capture warning for
/// a job the runner had already accepted. A read still stuck after this long
/// is treated as unreadable.
const RUNON_CAPTURE_READ_GRACE: Duration = Duration::from_secs(1);

/// Added to the promotion reply of a foreground `runon` call whose window
/// closed before the runner accepted the job.
pub(super) const REMOTE_ADMISSION_PENDING_NOTE: &str = "The remote job is queued for remote admission: the runner has not accepted it yet. It was not killed and keeps going, and its completion arrives on this task, so do not rerun the command.";

/// The reply for a runon task handed to the background before the runner has
/// accepted it. Foreground calls reach this at the end of their wait window;
/// explicit background calls reach it as soon as the task is registered and
/// keep background launch wording rather than claiming a foreground wait. Both
/// retain the admission-pending note. Every other reply passes through unchanged:
/// a finished or refused task, an accepted job, or a reply returned early because
/// AFT is restarting.
fn foreground_runon_reply(
    response: Response,
    window_end: Option<Instant>,
    background_runon: bool,
    cancel: &BashWaitCancel,
    timeout: Option<u64>,
    window_ms: u64,
    worker_session: bool,
    bash_watch_available: bool,
) -> Response {
    if (!background_runon && window_end.is_none())
        || (!background_runon && window_end.is_some_and(|window_end| Instant::now() < window_end))
        || cancel.drain.is_active()
        || !response.success
        || response.data["status"] != "starting"
    {
        return response;
    }
    let Some(task_id) = response.data["task_id"].as_str() else {
        return response;
    };
    let guidance = if background_runon {
        crate::commands::bash_orchestrate::format_background_launch(
            task_id,
            false,
            worker_session,
            bash_watch_available,
        )
    } else {
        crate::commands::bash_orchestrate::format_promotion_message(
            task_id,
            timeout,
            window_ms,
            worker_session,
            bash_watch_available,
        )
    };
    let reply = Response::success(
        &response.id,
        json!({
            "output": format!("{guidance}\n{REMOTE_ADMISSION_PENDING_NOTE}"),
            "task_id": task_id,
            "status": "starting",
            "remote_phase": crate::bash_background::registry::remote::RemotePhase::Submitting.tag(),
            "remote_job_id": Value::Null,
        }),
    );
    with_capture_warning(&reply)
}

#[allow(clippy::too_many_arguments)]
async fn await_runon_launch_reply(
    registry: crate::bash_background::BgTaskRegistry,
    session: &str,
    response: &Response,
    deadline: Option<Instant>,
    cancel: &BashWaitCancel,
    claim: &drain::BashCallClaim,
    server_completion: bool,
    reply_deadline: Option<Instant>,
    background_runon: bool,
) -> Option<Response> {
    let deadline = deadline?;
    let task_id = response.data.get("task_id")?.as_str()?.to_owned();
    let capture = async {
        if background_runon {
            return cached_runon_launch_reply(&registry, &task_id, session, response, true);
        }
        loop {
            let observed_registry = registry.clone();
            let observed_task = task_id.clone();
            let observed_session = session.to_owned();
            let launch = copy_response(response);
            // Task-state and artifact access can block on disk or mutexes. Keep it
            // off the frame runtime and release every lock before polling again.
            // Wait for the worker to settle before reading a terminal state, so a
            // refusal is rendered from what the worker finished recording.
            let observation = tokio::task::spawn_blocking(move || {
                (observed_registry.runon_launch_pending(&observed_task, &observed_session)
                    != Some(true))
                .then(|| {
                    settled_runon_launch_reply(
                        &observed_registry,
                        &observed_task,
                        &observed_session,
                        &launch,
                        background_runon,
                    )
                })
            });
            let read_deadline = if background_runon {
                Instant::now() + Duration::from_millis(100)
            } else {
                deadline.max(Instant::now() + RUNON_CAPTURE_READ_GRACE)
            };
            match tokio::time::timeout_at(read_deadline.into(), observation).await {
                Ok(Ok(Some(reply))) => return reply,
                Ok(Ok(None)) if background_runon => {
                    return Some(with_capture_warning(response));
                }
                Ok(Ok(None)) if Instant::now() < deadline => {
                    tokio::time::sleep_until(
                        (Instant::now() + Duration::from_millis(20))
                            .min(deadline)
                            .into(),
                    )
                    .await;
                }
                // The state read itself overran the deadline and its grace;
                // reading it again could block the reply just as long.
                Err(_) => {
                    return cached_runon_launch_reply(
                        &registry,
                        &task_id,
                        session,
                        response,
                        background_runon,
                    )
                }
                _ => {
                    return unsettled_runon_launch_reply(
                        registry.clone(),
                        task_id.clone(),
                        session.to_owned(),
                        response,
                        background_runon,
                    )
                    .await;
                }
            }
        }
    };
    tokio::select! {
        biased;
        _ = claim.module_loop_claimed() => None,
        _ = cancel.cancelled() => None,
        _ = async { tokio::time::sleep_until(reply_deadline.unwrap().into()).await }, if reply_deadline.is_some() => cached_runon_launch_reply(&registry, &task_id, session, response, background_runon),
        _ = async {
            while !cancel.drain.is_active() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }, if !server_completion => unsettled_runon_launch_reply(
            registry.clone(),
            task_id.clone(),
            session.to_owned(),
            response,
            background_runon,
        )
        .await,
        reply = capture => reply,
    }
}

fn finish_bash_spawn_immediate(
    response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    text_tx: &mut Option<oneshot::Sender<String>>,
    control_tx: &mut Option<oneshot::Sender<BashSpawnControl>>,
    allow_bg_completions: bool,
    repeat: &mut Option<crate::run_tool_call::RepeatObservation>,
) -> Response {
    let result = finalized_bash_result(
        response,
        ctx,
        session_id,
        format_context,
        allow_bg_completions,
        repeat.take(),
    );
    let ToolCallResult { text, response } = result;
    if let Some(tx) = text_tx.take() {
        let _ = tx.send(text);
    }
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashSpawnControl::Immediate);
    }
    response
}

fn finish_bash_poll_done(
    response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    text_tx: &mut Option<oneshot::Sender<String>>,
    control_tx: &mut Option<oneshot::Sender<BashPollControl>>,
    allow_bg_completions: bool,
) -> Response {
    let result = finalized_bash_result(
        response,
        ctx,
        session_id,
        format_context,
        allow_bg_completions,
        None,
    );
    let ToolCallResult { text, response } = result;
    if let Some(tx) = text_tx.take() {
        let _ = tx.send(text);
    }
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashPollControl::Done);
    }
    response
}

/// Combine the reply windows chosen by bash dispatch into the watchdog deadline.
pub(super) fn unanswered_bash_reply_deadline(
    received_at: Instant,
    wait_window_ms: u64,
    startup_window_ms: u64,
    capture_deadline: Option<Instant>,
    command_timeout_ms: u64,
) -> Instant {
    // Local command timeouts start at launch, not receipt. Allow startup before
    // that timeout while keeping the reply wait cap measured from receipt.
    let reply_wait_ms = wait_window_ms.min(startup_window_ms.saturating_add(command_timeout_ms));
    let mut deadline = received_at + Duration::from_millis(startup_window_ms.max(reply_wait_ms));
    if let Some(capture_deadline) = capture_deadline {
        deadline = deadline.max(capture_deadline);
    }
    deadline
}

#[cfg(test)]
mod unanswered_deadline_tests {
    use super::*;

    #[test]
    fn command_timeout_watchdog_includes_startup_but_respects_reply_cap() {
        let received_at = Instant::now();
        for (wait_ms, startup_ms, timeout_ms, capture_ms, expected_ms) in [
            (120_000, 5_000, 30_000, None, 35_000),
            (32_000, 5_000, 30_000, None, 32_000),
            (2_000, 5_000, 30_000, None, 5_000),
            (120_000, 5_000, 30_000, Some(60_000), 60_000),
            (120_000, 5_000, u64::MAX, None, 120_000),
        ] {
            let deadline = unanswered_bash_reply_deadline(
                received_at,
                wait_ms,
                startup_ms,
                capture_ms.map(|ms| received_at + Duration::from_millis(ms)),
                timeout_ms,
            );
            assert_eq!(
                deadline.duration_since(received_at),
                Duration::from_millis(expected_ms),
                "wait={wait_ms} startup={startup_ms} timeout={timeout_ms} capture={capture_ms:?}"
            );
        }
    }
}

/// A codemode program's shell call that would need the user's approval
/// (a scanned permission or a host escalation) is refused by name: nobody can
/// be asked on the head agent's behalf from a run route yet.
fn refuse_run_permission_ask(response: Response, run_scoped: bool) -> Response {
    if !run_scoped
        || response.success
        || response.data.get("code").and_then(Value::as_str)
            != Some(crate::protocol::ERROR_PERMISSION_REQUIRED)
    {
        return response;
    }
    Response::error_with_data(
        &response.id,
        crate::access_policy::PERMISSION_ASK_UNAVAILABLE,
        "this shell command needs permission, and a codemode program can't ask for it",
        json!({
            "permission": "bash",
            "resource": response.data.get("asks").cloned().unwrap_or(Value::Null),
        }),
    )
}

#[cfg(test)]
mod run_permission_tests {
    use super::*;

    fn permission_required() -> Response {
        Response::error_with_data(
            "run-shell",
            crate::protocol::ERROR_PERMISSION_REQUIRED,
            "bash command requires permission",
            json!({"asks": [{"kind": "escalation", "command": "make"}]}),
        )
    }

    #[test]
    fn run_shell_permission_ask_is_refused_permission_ask_unavailable() {
        let refused = refuse_run_permission_ask(permission_required(), true);
        assert!(!refused.success);
        assert_eq!(refused.data["code"], "permission_ask_unavailable");
        assert_eq!(refused.data["permission"], "bash");
        assert_eq!(
            refused.data["resource"],
            json!([{"kind": "escalation", "command": "make"}])
        );
        // The head's direct call keeps its ordinary approval request.
        let direct = refuse_run_permission_ask(permission_required(), false);
        assert_eq!(
            direct.data["code"],
            crate::protocol::ERROR_PERMISSION_REQUIRED
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn submit_deferred_bash(
    handles_progress: bool,
    executor: &Arc<Executor>,
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    metrics: &Arc<DispatchPathMetrics>,
    dispatch: DispatchFn,
    root: ProjectRootId,
    project_root: PathBuf,
    session_id: String,
    request_id: String,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    arguments: Value,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
    bind_trust: BindTrust,
    spawn_principal: crate::sandbox_spawn::AuthenticatedPrincipal,
    edit_slot_survives: Option<bool>,
    call_key: Option<String>,
    // The v1 `bash` schema pin the call carried, if any. It is installed
    // around the dispatch (see `with_schema_pin`) so the spawned task
    // records it; a pinless call leaves no view record.
    schema_pin: Option<String>,
    permissions_granted: Option<Vec<String>>,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
    worker_session: bool,
    worker_preset: bool,
    worker_jail: bool,
    server_completion: bool,
    remote_source: super::remote_policy::RemoteSource,
    reply_budget: impl Into<crate::commands::blocking_wait::ReplyBudget>,
) {
    let reply_budget = reply_budget.into();
    let received_at = reply_budget.received_at;
    // Granted permissions resume a call after a prompt, which may outlast the
    // startup allowance. Start admission now, but keep the caller's reply clock.
    let startup_started_at = if permissions_granted.is_some() {
        Instant::now()
    } else {
        received_at
    };
    #[cfg(test)]
    let timing_for_test = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone());
    #[cfg(test)]
    let spawn_timing_for_test = timing_for_test.clone();
    let run_scope = cancel.run_scope.clone();
    let scope_stamp = cancel.scope.clone();
    let speech_seed = cancel.speech_seed.clone();
    let secret_identity = super::secret_consent::current_identity();
    let secret_scopes = secret_identity
        .as_ref()
        .map(super::secret_consent::call_scopes);
    let run_scoped = run_scope.is_some();
    let server_completion = server_completion || run_scoped;
    // Local startup must answer early enough for its reply to be queued and
    // relayed before the client's deadline. OpenCode and Pi extend their reply
    // deadlines for runon calls to allow input capture after task registration.
    let grant_scope = crate::access_policy::current_scope();
    let grant_denial = crate::access_policy::scope_grants_denial();
    let ask_route = crate::access_policy::current_ask_route();
    // The attested scope decides confinement independently of consent routing:
    // heads and workers both ask, but only worker shells require the jail.
    let spawn_ctx = match executor.try_actor_context(&root) {
        Some(Some(ctx)) => ctx,
        state => {
            let code = if state.is_none() {
                "executor_busy"
            } else {
                "actor_not_registered"
            };
            let response = Response::error(
                &request_id,
                code,
                "bash refused before startup: executor admission state is unavailable",
            );
            let result = bash_result_from_response(response, &format_context);
            let completion_tx = completion_tx.clone();
            let metrics = metrics.clone();
            tokio::spawn(async move {
                send_bash_deferred_completion(
                    &completion_tx,
                    &metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root,
                    request_id,
                    Some(result),
                    false,
                )
                .await;
            });
            return;
        }
    };
    if matches!(ask_route, crate::access_policy::AskRoute::Plugin { .. }) {
        spawn_ctx.mark_standalone_plugin_host();
    }
    // Capture unavailable grants on the executor, where database reads belong,
    // and annotate only the reply being delivered, not each foreground poll.
    let notice_snapshot = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let snapshot_for_reply = notice_snapshot.clone();
    let notice_state = spawn_ctx.launch_grant_state.clone();
    let notice_key = match &ask_route {
        crate::access_policy::AskRoute::Consent { launch, .. } => launch.clone(),
        _ => None,
    };
    let annotated_completion_tx = notice_key.map(|key| {
        let (tx, mut annotated_completion_rx) = mpsc::channel::<BashDeferredCompletion>(1);
        let completion_forward = completion_tx.clone();
        let completion_metrics = metrics.clone();
        tokio::spawn(async move {
            while let Some(mut done) = annotated_completion_rx.recv().await {
                decrement_counted_channel(&completion_metrics.bash_deferred_queued);
                if let Some(result) = done.result.as_mut() {
                    let notices = notice_state.notices(&key, &snapshot_for_reply.lock());
                    crate::context::launch_grants::annotate_shell_reply(
                        &mut result.text,
                        &mut result.response,
                        &notices,
                    );
                }
                let _ = send_counted_channel(
                    &completion_forward,
                    &completion_metrics.bash_deferred_queued,
                    done,
                )
                .await;
            }
        });
        tx
    });
    let completion_tx = annotated_completion_tx.as_ref().unwrap_or(completion_tx);
    let raw_args = arguments
        .get("params")
        .filter(|params| params.is_object())
        .unwrap_or(&arguments);
    let blocking_intent = ["wait", "block_to_completion"].iter().any(|key| {
        raw_args
            .get(*key)
            .and_then(crate::subc_translate::model_boolean)
            == Some(true)
    });
    let capture_wait_mode = raw_args
        .get("wait")
        .and_then(crate::subc_translate::model_boolean)
        == Some(true)
        && !server_completion;
    let worker_wait_max_ms = crate::commands::bash_orchestrate::worker_wait_max_ms(&spawn_ctx);
    let blocking_cap_ms = crate::commands::bash_orchestrate::blocking_wait_cap_ms(
        blocking_intent,
        worker_wait_max_ms,
    )
    .map(|cap| {
        cap.min(
            reply_budget
                .deadline
                .saturating_duration_since(received_at)
                .as_millis() as u64,
        )
    });
    let capture_deadline = raw_args
        .get("runon")
        .filter(|value| !value.is_null())
        .map(|_| {
            received_at
                + raw_args
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .map(Duration::from_millis)
                    .unwrap_or(REMOTE_INPUT_CAPTURE_WAIT)
                    .min(REMOTE_INPUT_CAPTURE_WAIT)
                    .min(
                        blocking_cap_ms
                            .map(Duration::from_millis)
                            .unwrap_or(REMOTE_INPUT_CAPTURE_WAIT),
                    )
        });
    #[cfg(test)]
    let capture_deadline = capture_deadline.map(|deadline| {
        timing_for_test
            .as_ref()
            .and_then(|timing| timing.capture_wait)
            .map_or(deadline, |wait| received_at + wait)
    });
    let capture_deadline = capture_deadline.map(|deadline| reply_budget.cap(deadline));
    // The short foreground window belongs only to nonblocking calls. A blocking
    // call keeps the existing 15 s startup admission budget, then waits for a
    // committed command until its blocking cap, even if startup bookkeeping is slow.
    let startup_window = if server_completion {
        20_000
    } else if blocking_intent {
        15_000.min(blocking_cap_ms.expect("blocking wait cap"))
    } else {
        crate::commands::bash_orchestrate::resolve_foreground_wait_window_ms(
            spawn_ctx.config().foreground_wait_window_ms,
        )
    }
    .min(20_000);
    // A nonblocking foreground `runon` call answers within the same foreground
    // window as a local command, counted from when the call arrived, never
    // from when the runner accepts the job. The runner can take far longer than
    // the window to accept a job when it is busy, and the host plugins give an
    // ordinary foreground call only the window plus a short margin before they
    // give up. A caller that gives up gets no task id, cannot tell whether the
    // job ran, and may run it a second time. So the wait for the runner's
    // acceptance ends at the window, and a job still unaccepted by then is
    // handed to the background with its task id (`foreground_runon_reply`).
    let background_runon = capture_deadline.is_some()
        && raw_args
            .get("background")
            .and_then(crate::subc_translate::model_boolean)
            == Some(true);
    let foreground_runon = capture_deadline.is_some()
        && !blocking_intent
        && !server_completion
        && !["background", "pty"].iter().any(|key| {
            raw_args
                .get(*key)
                .and_then(crate::subc_translate::model_boolean)
                == Some(true)
        });
    let foreground_runon_deadline = foreground_runon
        .then(|| reply_budget.cap(received_at + Duration::from_millis(startup_window)));
    let capture_deadline = capture_deadline
        .map(|deadline| foreground_runon_deadline.map_or(deadline, |window| deadline.min(window)));
    // The capture wait's state reads get a short grace past the window (see
    // `RUNON_CAPTURE_READ_GRACE`); a read still blocked then must not hold a
    // foreground reply any longer.
    let capture_reply_deadline = foreground_runon_deadline
        .map_or(reply_budget.deadline, |window| {
            reply_budget.cap(window + RUNON_CAPTURE_READ_GRACE)
        });
    let foreground_runon_timeout = raw_args.get("timeout").and_then(Value::as_u64);
    let startup_deadline =
        reply_budget.cap(startup_started_at + Duration::from_millis(startup_window));
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new_with_start_time(
        startup_deadline,
        startup_started_at,
    ));
    let changed_files_budget = (spawn_ctx.config().bash.changed_files
        != crate::config::BashChangedFiles::Off
        && (server_completion
            || !["background", "pty"].iter().any(|key| {
                raw_args
                    .get(*key)
                    .and_then(crate::subc_translate::model_boolean)
                    == Some(true)
            })))
    .then(|| crate::bash_changed_files::BudgetGuard::new(reply_budget.deadline));
    if let Some(budget) = &changed_files_budget {
        receipt.bind_changed_files_deadline(budget.shared());
    }
    // Bind before dispatch so even a command that exits inside spawn sees the
    // caller's deadline. Clear on the reply channel, including startup handoffs
    // whose executor job can still be finishing after the caller has its task id.
    let changed_files_completion_tx = changed_files_budget.map(|budget| {
        let (tx, mut rx) = mpsc::channel::<BashDeferredCompletion>(1);
        let forward = completion_tx.clone();
        let budget_metrics = metrics.clone();
        tokio::spawn(async move {
            while let Some(done) = rx.recv().await {
                decrement_counted_channel(&budget_metrics.bash_deferred_queued);
                budget.clear();
                let _ = send_counted_channel(&forward, &budget_metrics.bash_deferred_queued, done)
                    .await;
            }
        });
        tx
    });
    let completion_tx = changed_files_completion_tx
        .as_ref()
        .unwrap_or(completion_tx);
    let receipt_for_spawn = Arc::clone(&receipt);
    let admitted = Arc::new(AtomicBool::new(false));
    let admitted_for_spawn = Arc::clone(&admitted);
    let claim = metrics.held_bash_calls.insert(route, corr);
    if handles_progress {
        metrics
            .held_bash_calls
            .record_progress(route, corr, &spawn_ctx, &request_id);
    }
    let (spawn_control_tx, spawn_control_rx) = oneshot::channel::<BashSpawnControl>();
    let (spawn_text_tx, spawn_text_rx) = oneshot::channel::<String>();
    let root_for_spawn = root.clone();
    let request_id_for_spawn = request_id.clone();
    let session_for_spawn = session_id.clone();
    let project_root_for_spawn = project_root.clone();
    let format_context_for_spawn = format_context.clone();
    // A call that returns its result ends in exactly one of three places: the
    // spawn job answers it at once (a rewritten command, an error, or a
    // background launch), a poll job answers it when the foreground wait
    // finishes, or the promote job answers it when the wait window closes. Each
    // gets its own copy; only the one that answers observes the call. A held
    // call the module loop answers instead (drain or cancel) carries no result
    // and is not observed.
    let mut repeat_for_spawn = repeat.clone();
    let ledger_carrier = cancel
        .ledger_carrier
        .clone()
        .unwrap_or_else(|| super::tool_provider::ledger_carrier(&spawn_principal));
    let ledger_key =
        call_key
            .as_ref()
            .filter(|_| server_completion)
            .map(|key| crate::db::call_ledger::Key {
                carrier: ledger_carrier.clone(),
                call_key: key.clone(),
            });
    // A task records the same requester as its call's ledger row.
    let task_requester = cancel.ledger_carrier.clone();
    let submit_executor = executor.clone();
    let unanswered_metrics = Arc::clone(metrics);
    let spawn_cancel = JobCancellation::new();
    let submit_cancel = spawn_cancel.clone();
    let submit_request_id = request_id.clone();
    // Even submission can wait for a scheduler lock. The reply timer below
    // lives on the frame runtime; submission and every spawn syscall do not.
    let spawn_submission = tokio::task::spawn_blocking(move || {
        submit_executor.submit_tool_call_with_cancellation_async(
            root_for_spawn,
            Lane::Mutating,
            submit_request_id,
            "bash",
            Box::new(move |ctx| {
                let _progress = crate::context::progress_origin_guard(
                    &request_id_for_spawn, route.channel, route.epoch, corr,
                );
                log_ctx::with_session(Some(session_for_spawn.clone()), || {
                    admitted_for_spawn.store(true, Ordering::Relaxed);
                    receipt_for_spawn.enter_startup_step(
                        crate::bash_background::BashStartupStep::Setup,
                    );
                    if receipt_for_spawn.refused() {
                        return Response::error(
                            &request_id_for_spawn,
                            "bash_start_deadline",
                            "bash startup deadline expired while queued for executor admission",
                        );
                    }
                    let mut spawn_text_tx = Some(spawn_text_tx);
                    let mut spawn_control_tx = Some(spawn_control_tx);

                    if matches!(bind_trust, BindTrust::Untrusted) && permissions_granted.is_none() {
                        let response = bash_denied_untrusted_response(request_id_for_spawn.clone());
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            false,
                            &mut repeat_for_spawn,
                        );
                    }

                    // Native bash skips the normal tool-call registration flow, so update the
                    // registration before rewriting. Rewritten reads then match ordinary reads.
                    if edit_slot_survives.is_some() {
                        crate::run_tool_call::ensure_hashline_registration(
                            ctx,
                            &project_root_for_spawn,
                            &session_for_spawn,
                            edit_slot_survives,
                            false,
                        );
                    }

                    let mut translated = match crate::subc_translate::subc_translate_owned(
                        "bash",
                        arguments,
                        &project_root_for_spawn,
                    ) {
                        Ok(translated) => translated,
                        Err(error) => {
                            let response = Response::error(
                                request_id_for_spawn.clone(),
                                error.code,
                                error.message,
                            );
                            return finish_bash_spawn_immediate(
                                response,
                                ctx,
                                &session_for_spawn,
                                &format_context_for_spawn,
                                &mut spawn_text_tx,
                                &mut spawn_control_tx,
                                true,
                                &mut repeat_for_spawn,
                            );
                        }
                    };
                    if let Some(grants) = permissions_granted {
                        translated
                            .args
                            .insert("permissions_requested".to_string(), Value::Bool(true));
                        translated.args.insert(
                            "permissions_granted".to_string(),
                            Value::Array(grants.into_iter().map(Value::String).collect()),
                        );
                    }
                    // `worker_session` comes from the plugin-set field of the subc
                    // call body, never from the agent's arguments: an agent must not be able to claim it is a
                    // worker to lift the default hard kill on its command.
                    translated
                        .args
                        .remove(crate::protocol::WORKER_SESSION_FIELD);
                    if worker_session {
                        translated.args.insert(
                            crate::protocol::WORKER_SESSION_FIELD.to_string(),
                            Value::Bool(true),
                        );
                    }
                    // Likewise the caller's tool names, taken from the call body
                    // (kept on the call's format context), never from the arguments.
                    translated
                        .args
                        .remove(crate::host_tool_names::HOST_TOOL_NAMES_FIELD);
                    if let Some(names) = &format_context_for_spawn.host_tool_names {
                        translated.args.insert(
                            crate::host_tool_names::HOST_TOOL_NAMES_FIELD.to_string(),
                            serde_json::json!(names),
                        );
                    }
                    if let Some(scope) = &run_scope {
                        if let Err(error) = scope.cached_budget().and_then(|budget| budget.constrain_bash(&mut translated.args)) {
                            return finish_bash_spawn_immediate(
                                Response::error(&request_id_for_spawn, &error.code, error.message),
                                ctx, &session_for_spawn, &format_context_for_spawn,
                                &mut spawn_text_tx, &mut spawn_control_tx, true, &mut repeat_for_spawn,
                            );
                        }
                    }
                    let settings = bash_settings_from_translated(&translated.args);
                    let command_timeout_ms = settings.timeout.unwrap_or_else(|| {
                        crate::bash_background::registry::DEFAULT_BG_TIMEOUT.as_millis() as u64
                    });
                    let raw_req = RawRequest { id: request_id_for_spawn.clone(),
                    command: "bash".to_string(),
                    lsp_hints: None,
                    session_id: Some(session_for_spawn.clone()),
                    params: Value::Object(translated.args), };
                    let (response, storage_dir) =
                        crate::sandbox_spawn::with_authenticated_principal(spawn_principal, || {
                            // Declared first so its test-only drop check runs after
                            // both guards restore this exact executor thread.
                            #[cfg(test)]
                            let _secret_restored = super::secret_consent::SecretContextRestoreCheck::new();
                            let _secret_identity = super::secret_consent::SecretIdentityGuard::install_optional(secret_identity);
                            let _secret_scopes = super::secret_consent::SecretScopeGuard::install(secret_scopes);
                            let _grant_scope = crate::access_policy::scope_guard(grant_scope);
                            let _grants = crate::access_policy::scope_grants_guard(grant_denial);
                            let _ask_route = crate::access_policy::ask_route_guard(Some(ask_route));
                            crate::sandbox_spawn::with_worker_jail(worker_jail, || {
                            crate::bash_background::with_call_key(call_key, || {
                                crate::bash_background::registry::with_ledger_spawn(
                                    ledger_key.clone(),
                                    || {
                                        (
                                            crate::bash_background::with_spawn_receipt(
                                                Arc::clone(&receipt_for_spawn),
                                                || {
                                                    crate::bash_background::with_worker_preset(
                                                        worker_preset,
                                                        || crate::bash_background::with_remote_policy(
                                                            super::remote_policy::lookup(
                                                                ctx,
                                                                &remote_source,
                                                            ),
                                                         || {
                                                              crate::bash_background::registry::with_scope_spawn(scope_stamp.clone(), || crate::gh_shim_speech::service::with_launch(speech_seed.clone(),&ctx.storage_dir(),|| {
                                                                  let run_dispatch = || {
                                                                      crate::bash_rewrite::with_rewrites_off(run_scoped, || {
                                                                           crate::bash_background::with_call_requester(task_requester, || {
                                                                               match ctx.unrestored_launch_grants(&raw_req.id) {
                                                                                   Ok(grants) => { *notice_snapshot.lock() = grants; dispatch(raw_req, ctx) }
                                                                                   Err(response) => response,
                                                                               }
                                                                           })
                                                                      })
                                                                  };
                                                                  #[cfg(not(test))]
                                                                  let response = crate::bash_background::with_schema_pin(schema_pin, run_dispatch);
                                                                  #[cfg(test)]
                                                                  let response = with_bash_timing_for_test(spawn_timing_for_test.clone(), || crate::bash_background::with_schema_pin(schema_pin, run_dispatch));
                                                                  let response = refuse_run_permission_ask(response, run_scoped);
                                                                  // Registration and revocation can race across actors.
                                                                  // Recheck after dispatch so neither ordering loses the task.
                                                                  if let Some(scope) = &scope_stamp {
                                                                      ctx.bash_background().kill_scope_if_revoked(scope);
                                                                  }
                                                                  response
                                                              }))
                                                         },
                                                        ),
                                                    )
                                                },
                                            ),
                                            crate::bash_background::task_storage_dir(ctx),
                                        )
                                    },
                                )
                            })
                            })
                        });
                    if let (Some(key), Some(db)) = (&ledger_key, ctx.db()) {
                        if let Ok(conn) = db.lock() {
                            if let Err(error) =
                                crate::db::call_ledger::note_native_outcome(&conn, key, &response)
                            {
                                log::warn!("call ledger shell outcome recording failed: {error}");
                            }
                        }
                    }
                    if !response.success {
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                            &mut repeat_for_spawn,
                        );
                    }

                    let Some(task_id) = response
                        .data
                        .get("task_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                            &mut repeat_for_spawn,
                        );
                    };
                    let mode = response
                        .data
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("pipes");
                    let is_pty = mode == "pty" || settings.pty;
                    // A codemode program's background launch answers at once like
                    // a classic one; its task stays under the run's lifetime.
                    let launch_answers_now =
                        !server_completion || (run_scoped && settings.background);
                    if launch_answers_now && (is_pty || settings.background) {
                        let unanswered_deadline = unanswered_bash_reply_deadline(
                            received_at,
                            0,
                            startup_window,
                            capture_deadline,
                            command_timeout_ms,
                        );
                        unanswered_metrics.set_bash_unanswered_deadline(
                            route,
                            corr,
                            unanswered_deadline,
                        );
                        // A terminal spawn reply still has no display text;
                        // format the launch guidance before answering the host.
                        let mut launch_response = bash_background_launch_response(
                            &request_id_for_spawn,
                            &task_id,
                            is_pty,
                            worker_session,
                            format_context_for_spawn
                                .bash_watch_available
                                .unwrap_or(worker_session),
                            &crate::commands::bash_orchestrate::kill_deadline_note(
                                ctx.bash_background(),
                                &task_id,
                                &session_for_spawn,
                                worker_session,
                            ),
                        );
                        launch_response.data["status"] = response.data["status"].clone();
                        return finish_bash_spawn_immediate(
                            launch_response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                            &mut repeat_for_spawn,
                        );
                    }

                    // The blocking wait cap limits every caller's reply wait,
                    // regardless of the route's cancellation policy. Reaching
                    // that cap hands back a task id without killing the command;
                    // the command's own timeout still governs its lifetime.
                    let worker_cap_ms = if run_scoped { None } else { blocking_cap_ms };
                    let wait_window_ms = worker_cap_ms.unwrap_or_else(|| {
                        crate::commands::bash_orchestrate::select_foreground_wait_window_ms(
                            ctx.config().foreground_wait_window_ms,
                            settings.timeout,
                            settings.wait,
                        )
                    });
                    let wait_window_ms =
                        if settings.wait || settings.block_to_completion || server_completion {
                            wait_window_ms
                        } else {
                            // Shorten only the execution wait in promotion probes;
                            // the generic foreground override also fences startup.
                            #[cfg(any(test, feature = "test-timing-hooks"))]
                            let wait_window_ms =
                                std::env::var("AFT_TEST_SUBC_BASH_PROMOTION_WAIT_MS")
                                    .ok()
                                    .and_then(|value| value.parse::<u64>().ok())
                                    .unwrap_or(wait_window_ms);
                            wait_window_ms.min(20_000)
                        };
                    let unanswered_deadline = unanswered_bash_reply_deadline(
                        received_at,
                        wait_window_ms,
                        startup_window,
                        capture_deadline,
                        command_timeout_ms,
                    );
                    unanswered_metrics.set_bash_unanswered_deadline(route, corr, unanswered_deadline);
                    let deadline = reply_budget.cap(received_at + Duration::from_millis(wait_window_ms));
                    receipt_for_spawn.limit_changed_files_deadline(deadline);
                    // Server-owned calls use the outer transport timer rather
                    // than this ordinary remote-job handback timer.
                    let remote_handback = ((settings.block_to_completion || settings.wait)
                        && !server_completion)
                        .then(|| {
                            let waited_ms =
                                crate::commands::bash_orchestrate::remote_block_handback_ms(
                                    settings.timeout,
                                    worker_cap_ms,
                                );
                            (reply_budget.cap(received_at + Duration::from_millis(waited_ms)), waited_ms)
                        });
                    let project_root = ctx.config().project_root.clone();
                    // Register the session as detachable exactly like the
                    // standalone path (bash_orchestrate) does: without this, a
                    // bash_wait_detach signal finds no active wait and wait:true
                    // blocks through user messages.
                    let detach_on_user_message = settings.wait && !server_completion;
                    ctx.bash_background()
                        .register_foreground_task(&session_for_spawn, &task_id);
                    if detach_on_user_message {
                        ctx.bash_background()
                            .begin_wait_mode_session(&session_for_spawn, &task_id);
                    }
                    // The watchdog can beat a queued poll or promotion. Prepare
                    // the same blocking-wait guidance while task state is still
                    // available, rather than reading it on the deadline path.
                    let blocking_handoff_response = worker_cap_ms.map(|cap| {
                        let kill_note = crate::commands::bash_orchestrate::kill_deadline_note(
                            ctx.bash_background(), &task_id, &session_for_spawn, worker_session,
                        );
                        Response::success(&request_id_for_spawn, json!({
                            "output": format!(
                                "{}\nKill deadline recorded at launch (worker waits may extend the default):{kill_note}",
                                crate::commands::bash_orchestrate::format_worker_wait_limit_message(
                                    &task_id, cap, cap, None,
                                    format_context_for_spawn.bash_watch_available.unwrap_or(worker_session),
                                ),
                            ),
                            "duration_ms": cap,
                            "task_id": task_id,
                            "status": "running",
                        }))
                    });
                    if let Some(tx) = spawn_control_tx.take() {
                        let _ = tx.send(BashSpawnControl::Foreground {
                            task_id,
                            session_id: session_for_spawn.clone(),
                            project_root,
                            storage_dir,
                            deadline,
                            // A capped blocking wait detaches at its deadline.
                            block_to_completion: (settings.block_to_completion || settings.wait)
                                && worker_cap_ms.is_none(),
                            blocking_intent: settings.block_to_completion || settings.wait,
                            timeout: settings.timeout,
                            wait_window_ms,
                            detach_on_user_message,
                            worker_session,
                            worker_cap_ms,
                            blocking_handoff_response,
                            remote_handback,
                        });
                    }
                    response
                })
            }),
            submit_cancel,
        )
    });

    let executor = Arc::clone(executor);
    let completion_tx = completion_tx.clone();
    let poll_touch_tx = poll_touch_tx.clone();
    let task_metrics = Arc::clone(metrics);
    let root_for_task = root.clone();
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&task_metrics);
        // A task exists before the runner copies inputs. Publish its detach
        // target now so cancellation and module replacement can return its ID.
        let register_capture_target = |response: &Response| {
            let task_id = response.data.get("task_id")?.as_str()?.to_owned();
            let target = drain::BashDetachTarget {
                reply_cache: spawn_ctx
                    .bash_background()
                    .drain_reply_cache(&task_id, &session_id),
                run_scoped,
                task_id,
                session_id: session_id.clone(),
                wait_mode: capture_wait_mode,
                worker_session,
                server_completion,
                late_results: cancel.late_results,
                registry: spawn_ctx.bash_background().clone(),
                request_id: request_id.clone(),
                ver,
                flags,
                format_context: format_context.clone(),
            };
            task_metrics
                .held_bash_calls
                .set_detach_target(route, corr, target.clone());
            let capture_phase = if capture_wait_mode {
                drain::BashHoldPhase::Wait
            } else if blocking_intent {
                drain::BashHoldPhase::Block
            } else {
                drain::BashHoldPhase::Foreground
            };
            task_metrics
                .held_bash_calls
                .set_phase(route, corr, capture_phase);
            if let Ok(guard) = task_metrics.lifecycle_probe.lock() {
                if let Some(probe) = guard.as_ref() {
                    probe.bash_foreground_wait(route, corr, capture_phase.label());
                }
            }
            #[cfg(test)]
            if let Some(timing) = &timing_for_test {
                timing.capture_registered.notify_one();
            }
            Some(target)
        };
        let mut capture_target = None;
        let spawn_request_id = request_id.clone();
        let spawn_future = async move {
            match spawn_submission.await {
                Ok(rx) => await_executor_response(rx, spawn_request_id.clone()).await,
                Err(error) => Response::error(
                    &spawn_request_id,
                    "execution_failed",
                    format!("bash submission worker failed: {error}"),
                ),
            }
        };
        tokio::pin!(spawn_future);
        let mut answered_startup = false;
        let spawn_response = tokio::select! {
            biased;
            response = &mut spawn_future => response,
            _ = async {
                #[cfg(test)]
                if let Some(timing) = &timing_for_test {
                    return timing.startup.wait().await;
                }
                #[cfg(feature = "test-timing-hooks")]
                match receipt.startup_clock_for_test() {
                    // Execution-handoff fixtures test commands that have started.
                    // Let control/output files, process creation and task records
                    // finish first; a startup refusal tests a different outcome.
                    Some(crate::bash_background::StartupClockForTest::Frozen) => {
                        return std::future::pending::<()>().await;
                    }
                    Some(crate::bash_background::StartupClockForTest::Expired) => return,
                    None => {},
                }
                tokio::time::sleep_until(startup_deadline.into()).await;
            } => {
                // Settling the receipt is the one synchronization point with
                // the startup job: it takes the receipt lock that every commit
                // takes, so the outcome read here is final (see `SpawnReceipt`).
                let outcome = receipt.expire();
                if outcome == crate::bash_background::StartupOutcome::Refused {
                    spawn_cancel.request_cancel();
                }
                // An in-process rewrite has no task id to hand off, so its
                // reply is the only way to learn what it did. It is normally a
                // file read or a small edit; give it a short bounded wait.
                let settled = if outcome == crate::bash_background::StartupOutcome::Inline {
                    #[cfg(test)]
                    if let Some(timing) = &timing_for_test {
                        tokio::select! {
                            biased;
                            response = &mut spawn_future => Some(response),
                            _ = timing.inline_settle.wait() => None,
                        }
                    } else {
                        tokio::time::timeout_at(reply_budget.cap(Instant::now() + INLINE_STARTUP_SETTLE).into(), &mut spawn_future).await.ok()
                    }
                    #[cfg(not(test))]
                    tokio::time::timeout_at(reply_budget.cap(Instant::now() + INLINE_STARTUP_SETTLE).into(), &mut spawn_future).await.ok()
                } else if !server_completion && matches!(outcome, crate::bash_background::StartupOutcome::Task(_)) && blocking_cap_ms.is_some() {
                    let wait_deadline = reply_budget.cap(received_at + Duration::from_millis(blocking_cap_ms.unwrap()));
                    tokio::time::timeout_at(wait_deadline.into(), &mut spawn_future).await.ok()
                } else {
                    None
                };
                match settled {
                    Some(response) => response,
                    None => {
                        let startup_steps = receipt.startup_step_snapshot();
                        let response = startup_deadline_response(&request_id, &outcome, startup_window, blocking_cap_ms, server_completion, worker_session, format_context.bash_watch_available.unwrap_or(worker_session), &executor, &root_for_task, admitted.load(Ordering::Relaxed), startup_steps.current());
                        capture_target = register_capture_target(&response);
                        let response = if blocking_intent && capture_deadline.is_some() {
                            if let Some(task_id) = response.data["task_id"].as_str().map(str::to_owned) {
                                tokio::time::timeout_at(reply_budget.deadline.into(), warn_if_runon_capture_missing(spawn_ctx.bash_background().clone(), task_id, session_id.clone(), copy_response(&response))).await.unwrap_or_else(|_| with_capture_warning(&response))
                            } else { response }
                        } else {
                            let reply = await_runon_launch_reply(spawn_ctx.bash_background().clone(), &session_id, &response, capture_deadline, &cancel, &claim, server_completion, Some(capture_reply_deadline), background_runon).await.unwrap_or(response);
                            foreground_runon_reply(reply, foreground_runon_deadline, background_runon, &cancel, foreground_runon_timeout, startup_window, worker_session, format_context.bash_watch_available.unwrap_or(worker_session))
                        };
                        // This task runs on the shared subc runtime outside any session scope.
                        crate::log_ctx::with_session(Some(session_id.to_string()), || {
                            crate::slog_warn!("bash startup reply deadline channel={} corr={corr} elapsed_ms={} admitted={} code={} steps={}", route.channel, received_at.elapsed().as_millis(), admitted.load(Ordering::Relaxed), response.data.get("code").and_then(Value::as_str).unwrap_or("promoted"), startup_steps.format_steps());
                        });
                        let result = (!cancel.connection.is_cancelled() && !cancel.route.is_cancelled() && claim.try_claim_for_wait_task()).then(|| bash_result_from_response(response, &format_context));
                        send_bash_deferred_completion(&completion_tx, &task_metrics, route, corr, flags, ver, root_for_task.clone(), request_id.clone(), result, false).await;
                        answered_startup = true;
                        (&mut spawn_future).await
                    }
                }
            }
        };
        let spawn_control = spawn_control_rx.await;
        let blocking_wait = matches!(
            &spawn_control,
            Ok(BashSpawnControl::Foreground {
                blocking_intent: true,
                ..
            })
        );
        if !answered_startup && !blocking_wait && capture_deadline.is_some() {
            capture_target = register_capture_target(&spawn_response);
            if let Some(response) = await_runon_launch_reply(
                spawn_ctx.bash_background().clone(),
                &session_id,
                &spawn_response,
                capture_deadline,
                &cancel,
                &claim,
                server_completion,
                Some(capture_reply_deadline),
                background_runon,
            )
            .await
            {
                let response = foreground_runon_reply(
                    response,
                    foreground_runon_deadline,
                    background_runon,
                    &cancel,
                    foreground_runon_timeout,
                    startup_window,
                    worker_session,
                    format_context
                        .bash_watch_available
                        .unwrap_or(worker_session),
                );
                send_bash_deferred_completion(
                    &completion_tx,
                    &task_metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task.clone(),
                    request_id.clone(),
                    claim
                        .try_claim_for_wait_task()
                        .then(|| bash_result_from_response(response, &format_context)),
                    false,
                )
                .await;
                answered_startup = true;
            }
        }
        if !answered_startup
            && (claim.claimed_by_module_loop()
                || capture_target.is_some()
                    && (cancel.connection.is_cancelled() || cancel.route.is_cancelled()))
        {
            if claim.try_claim_for_wait_task() {
                if let Some(target) = capture_target.take() {
                    let _ =
                        tokio::task::spawn_blocking(move || detach_held_bash_sync(target, true))
                            .await;
                }
            }
            send_bash_deferred_completion(
                &completion_tx,
                &task_metrics,
                route,
                corr,
                flags,
                ver,
                root_for_task.clone(),
                request_id.clone(),
                None,
                false,
            )
            .await;
            return;
        }
        if answered_startup {
            // A slow startup may settle after its caller has already received
            // the task id. Promotion/persistence must not hold that reply open.
            if !claim.claimed_by_module_loop() {
                if let Ok(BashSpawnControl::Foreground {
                    task_id,
                    session_id,
                    detach_on_user_message,
                    ..
                }) = spawn_control
                {
                    let target = drain::BashDetachTarget {
                        reply_cache: spawn_ctx
                            .bash_background()
                            .drain_reply_cache(&task_id, &session_id),
                        run_scoped,
                        task_id,
                        session_id,
                        wait_mode: detach_on_user_message,
                        worker_session,
                        server_completion,
                        late_results: cancel.late_results,
                        registry: spawn_ctx.bash_background().clone(),
                        request_id,
                        ver,
                        flags,
                        format_context,
                    };
                    if blocking_cap_ms.is_some() && worker_session && !server_completion {
                        // Keep a delegated worker's implicit command timeout at least
                        // one wait interval away; an explicit timeout is never extended.
                        tokio::task::spawn_blocking(move || {
                            target.registry.renew_hard_kill(
                                &target.task_id,
                                &target.session_id,
                                Duration::from_millis(worker_wait_max_ms),
                            );
                            detach_held_bash_sync(target, false);
                        });
                    } else {
                        detach_held_bash_in_background(target, false);
                    }
                }
            }
            return;
        }
        match spawn_control {
            Ok(BashSpawnControl::Immediate) => {
                let text = spawn_text_rx.await.unwrap_or_else(|_| {
                    crate::subc_format::format_response_with_context(
                        "bash",
                        &spawn_response,
                        &format_context,
                    )
                });
                let result = ToolCallResult {
                    text,
                    response: spawn_response,
                };
                let fatal = response_is_fatal_panic(&result.response);
                send_bash_deferred_completion(
                    &completion_tx,
                    &task_metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    claim.try_claim_for_wait_task().then_some(result),
                    fatal,
                )
                .await;
            }
            Ok(BashSpawnControl::Foreground {
                task_id,
                session_id,
                project_root,
                storage_dir,
                deadline,
                block_to_completion,
                blocking_intent,
                timeout,
                wait_window_ms,
                detach_on_user_message,
                worker_session,
                worker_cap_ms,
                blocking_handoff_response,
                remote_handback,
            }) => {
                let phase = if detach_on_user_message {
                    drain::BashHoldPhase::Wait
                } else if blocking_intent {
                    drain::BashHoldPhase::Block
                } else {
                    drain::BashHoldPhase::Foreground
                };
                task_metrics.held_bash_calls.set_phase(route, corr, phase);
                let _deferred_wait = DeferredBashWaitGuard::new(&task_metrics);
                let deadline_target = drain::BashDetachTarget {
                    reply_cache: spawn_ctx
                        .bash_background()
                        .drain_reply_cache(&task_id, &session_id),
                    run_scoped,
                    task_id: task_id.clone(),
                    session_id: session_id.clone(),
                    wait_mode: detach_on_user_message,
                    worker_session,
                    server_completion,
                    late_results: cancel.late_results,
                    registry: spawn_ctx.bash_background().clone(),
                    request_id: request_id.clone(),
                    ver,
                    flags,
                    format_context: format_context.clone(),
                };
                let repeat_for_deadline = repeat.clone();
                let wait_future = run_deferred_bash_wait(
                    executor,
                    completion_tx.clone(),
                    poll_touch_tx,
                    task_metrics.clone(),
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task.clone(),
                    request_id.clone(),
                    task_id,
                    session_id,
                    project_root,
                    storage_dir,
                    deadline,
                    block_to_completion,
                    timeout,
                    wait_window_ms,
                    detach_on_user_message,
                    worker_session,
                    worker_cap_ms,
                    remote_handback,
                    format_context,
                    cancel,
                    claim.clone(),
                    repeat,
                    server_completion,
                );
                // Continue awaiting the executor result after sending the caller
                // a background task id: a later panic must still stop the module.
                // Dropping a JoinHandle leaves that observer running, unlike
                // dropping the future itself. Reserve the reply only once ready.
                let mut wait_task = tokio::spawn(wait_future);
                let reply_deadline = if server_completion {
                    reply_budget.deadline
                } else {
                    reply_budget.cap(deadline + PENDING_POLL_INTERVAL * 2)
                };
                tokio::select! {
                    biased;
                    _ = &mut wait_task => {}
                    _ = async {
                        #[cfg(test)]
                        if let Some(timing) = &timing_for_test {
                            timing.reply.wait().await;
                            if !timing.reply_uses_clock {
                                return;
                            }
                        }
                        wait_for_bash_reply_deadline(reply_deadline).await;
                    } => {
                        if claim.claim_for_deadline_handoff() {
                            let response = blocking_handoff_response.unwrap_or_else(|| deadline_handoff_response(&request_id, &deadline_target.task_id, wait_window_ms, worker_session, deadline_target.format_context.bash_watch_available.unwrap_or(worker_session)));
                            // Status-bar formatting can wait on diagnostic-cache or
                            // fleet-reader mutexes, so omit it from the deadline reply.
                            // Recording repeated calls only updates in-memory state.
                            let mut result = bash_result_from_response(response, &deadline_target.format_context);
                            if let Some(repeat) = repeat_for_deadline {
                                repeat.observe(&spawn_ctx, &mut result.text);
                            }
                            detach_held_bash_in_background(deadline_target, false);
                            send_bash_deferred_completion(&completion_tx, &task_metrics, route, corr, flags, ver, root_for_task, request_id, Some(result), false).await;
                        }
                    }
                }
            }
            Err(_) => {
                let result = bash_result_from_response(spawn_response, &format_context);
                let fatal = response_is_fatal_panic(&result.response);
                send_bash_deferred_completion(
                    &completion_tx,
                    &task_metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    Some(result),
                    fatal,
                )
                .await;
            }
        }
    });
}

async fn wait_for_bash_reply_deadline(deadline: Instant) {
    // Fixtures that test a reply produced by the wait task itself, such as a
    // failed promotion, hold this timer. Otherwise a slow runner lets the
    // deadline handoff answer first with its "still running" reply. While held,
    // the wait task's own reply is the only one, and finishing it ends the
    // select that awaits this timer.
    #[cfg(any(test, feature = "test-timing-hooks"))]
    if std::env::var("AFT_TEST_SUBC_BASH_REPLY_TIMER").as_deref() == Ok("held") {
        return std::future::pending::<()>().await;
    }
    tokio::time::sleep_until(deadline.into()).await;
    // Promotion probes require the executor job to start before racing its held
    // result against this timer. The two-second reply assertion includes this
    // rendezvous; calls without timing hooks keep the ordinary deadline.
    #[cfg(any(test, feature = "test-timing-hooks"))]
    if std::env::var_os("AFT_TEST_SUBC_BASH_PROMOTION_WAIT_MS").is_some() {
        if let Ok(path) = std::env::var("AFT_TEST_SUBC_BASH_PROMOTE_STARTED_FILE") {
            while !std::path::Path::new(&path).exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

fn deadline_handoff_response(
    request_id: &str,
    task_id: &str,
    wait_window_ms: u64,
    worker_session: bool,
    bash_watch_available: bool,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": crate::commands::bash_orchestrate::format_promotion_message(task_id, None, wait_window_ms, worker_session, bash_watch_available),
            "task_id": task_id, "status": "running",
        }),
    )
}

/// How long the startup reply deadline waits for an in-process bash rewrite
/// that committed before the deadline. The startup budget is at most 20 s
/// under the 25 s client transport class, so this keeps the reply inside it.
const INLINE_STARTUP_SETTLE: Duration = Duration::from_millis(1500);

/// The reply a bash call gets when its startup reply budget expires, built
/// from the receipt's final state. Every branch answers definitely whether the
/// command runs: refused means it was not started and never will be; a
/// committed task is handed off by id; anything else is a typed
/// outcome-unknown error that says how to find out.
#[allow(clippy::too_many_arguments)]
fn startup_deadline_response(
    request_id: &str,
    outcome: &crate::bash_background::StartupOutcome,
    startup_window: u64,
    blocking_cap_ms: Option<u64>,
    server_completion: bool,
    worker_session: bool,
    bash_watch_available: bool,
    executor: &Executor,
    root: &ProjectRootId,
    admitted: bool,
    slow_step: crate::bash_background::BashStartupStep,
) -> Response {
    use crate::bash_background::StartupOutcome;
    match outcome {
        StartupOutcome::Task(task_id) if !server_completion && blocking_cap_ms.is_some() => {
            let cap = blocking_cap_ms.unwrap();
            Response::success(request_id, json!({
                "output": crate::commands::bash_orchestrate::format_worker_wait_limit_message(
                    task_id, cap, cap, None, bash_watch_available,
                ),
                "duration_ms": cap,
                "task_id": task_id,
                "status": "running",
            }))
        }
        StartupOutcome::Task(task_id) if !server_completion => deadline_handoff_response(
            request_id,
            task_id,
            startup_window,
            worker_session,
            bash_watch_available,
        ),
        // A server-owned call is never handed off: once this reply goes out,
        // its local task is killed (a remote one keeps running). Either way
        // the command did start, so the caller must not treat it as not run.
        StartupOutcome::Task(task_id) => Response::error_with_data(
            request_id,
            "outcome_unknown_startup_deadline",
            format!(
                "bash startup exceeded its reply budget after the command started as task {task_id}; a server-owned call cannot be handed off, so a local task is stopped and may have run partly. Inspect it with bash_status {task_id}; never rerun the command automatically"
            ),
            json!({ "task_id": task_id }),
        ),
        StartupOutcome::Inline => Response::error(
            request_id,
            "outcome_unknown_startup_deadline",
            format!(
                "bash startup exceeded its reply budget while the command was running in-process as a rewrite (a file read, search or edit) and it did not finish within {} ms; it may have run, and it has no task id. Check the files it touches before rerunning it",
                INLINE_STARTUP_SETTLE.as_millis()
            ),
        ),
        StartupOutcome::Refused => Response::error(
            request_id,
            "bash_start_deadline",
            startup_refusal_reason(executor, root, admitted, slow_step),
        ),
    }
}

/// Only called once the receipt is refused: process creation and in-process
/// rewrites both commit through that receipt, and a refused receipt never
/// commits, so every reason here can say the command was not started.
fn startup_refusal_reason(
    executor: &Executor,
    root: &ProjectRootId,
    admitted: bool,
    slow_step: crate::bash_background::BashStartupStep,
) -> String {
    if admitted {
        return format!(
            "bash startup reply budget expired during {} before process creation committed; process creation is now refused, so the command was not started and will not start",
            slow_step.description()
        );
    }
    if let Some(writer) = executor
        .try_mutating_lane_snapshots()
        .and_then(|writers| writers.into_iter().find(|writer| &writer.root_id == root))
    {
        return format!("bash startup deadline: queued behind root Mutating job {} ({}); slow startup step: {}; command was not started", writer.request_id, writer.command, slow_step.description());
    }
    if executor
        .try_dispatch_liveness_snapshot()
        .is_some_and(|snapshot| {
            snapshot.running.interactive + snapshot.running.maintenance >= executor.pool_size()
        })
    {
        return format!("bash startup deadline: executor workers saturated; slow startup step: {}; command was not started", slow_step.description());
    }
    format!("bash startup deadline: queued for executor admission or scheduler lock; slow startup step: {}; command was not started", slow_step.description())
}

#[allow(clippy::too_many_arguments)]
async fn run_deferred_bash_wait(
    executor: Arc<Executor>,
    completion_tx: mpsc::Sender<BashDeferredCompletion>,
    poll_touch_tx: mpsc::Sender<ProjectRootId>,
    metrics: Arc<DispatchPathMetrics>,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    task_id: String,
    session_id: String,
    project_root: Option<PathBuf>,
    storage_dir: PathBuf,
    deadline: Instant,
    block_to_completion: bool,
    timeout: Option<u64>,
    wait_window_ms: u64,
    detach_on_user_message: bool,
    worker_session: bool,
    worker_cap_ms: Option<u64>,
    remote_handback: Option<(Instant, u64)>,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
    claim: Arc<drain::BashCallClaim>,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
    server_completion: bool,
) {
    let run_scoped = cancel.run_scope.is_some();
    let Some(wait_ctx) = executor.actor_context(&root) else {
        if claim.claimed_by_deadline() {
            return;
        }
        send_bash_deferred_completion(
            &completion_tx,
            &metrics,
            route,
            corr,
            flags,
            ver,
            root,
            request_id,
            None,
            false,
        )
        .await;
        return;
    };
    let registry = wait_ctx.bash_background().clone();
    let mut progress = wait_ctx.request_handles_progress(&request_id).then(|| {
        crate::bash_background::progress::BashProgressPump::new(
            &wait_ctx,
            task_id.clone(),
            session_id.clone(),
            request_id.clone(),
        )
    });
    // From here the module loop can answer this call and detach its command
    // without this task, should the executor polls below stall.
    metrics.held_bash_calls.set_detach_target(
        route,
        corr,
        drain::BashDetachTarget {
            reply_cache: registry.drain_reply_cache(&task_id, &session_id),
            run_scoped,
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            wait_mode: detach_on_user_message,
            worker_session,
            server_completion,
            late_results: cancel.late_results,
            registry: registry.clone(),
            request_id: request_id.clone(),
            ver,
            flags,
            format_context: format_context.clone(),
        },
    );
    // The phase was set when the spawn reported its foreground control (from
    // `blocking_intent`, not the capped `block_to_completion` flag this wait
    // receives); the detach target above is what moves the census off
    // `bash:spawning`. Publish the stored phase together with the detach so a
    // test waiting on this event sees exactly what a drain census taken next
    // would report for this call.
    if let Some(foreground_phase) = metrics.held_bash_calls.phase(route, corr) {
        if let Ok(guard) = metrics.lifecycle_probe.lock() {
            if let Some(probe) = guard.as_ref() {
                probe.bash_foreground_wait(route, corr, foreground_phase.label());
            }
        }
    }
    loop {
        tokio::select! {
            _ = claim.module_loop_claimed() => {
                // The module loop sent the terminal frame and owns the task's
                // registration; this completion only settles the accounting.
                send_bash_deferred_completion(
                    &completion_tx,
                    &metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root,
                    request_id,
                    None,
                    false,
                )
                .await;
                break;
            }
            _ = cancel.cancelled() => {
                if claim.claimed_by_deadline() {
                    break;
                }
                if claim.claim_for_wait_task() {
                    // Registry locks stay off the frame loop's thread.
                    let registry = registry.clone();
                    let session_id = session_id.clone();
                    let task_id = task_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if !run_scoped {
                            // The command keeps running as a task. Promote it so
                            // its completion is queued for the session like any
                            // background task's (a foreground task's completion
                            // is otherwise marked delivered the moment it ends,
                            // so a session that reconnects never got it), and
                            // tell the session about it on its next bash call.
                            if registry.promote(&task_id, &session_id).is_ok() {
                                registry.note_cancelled_call(&task_id, &session_id);
                            }
                        }
                        release_wait_registration(
                            &registry,
                            &session_id,
                            &task_id,
                            detach_on_user_message,
                        );
                    })
                    .await;
                }
                send_bash_deferred_completion(
                    &completion_tx,
                    &metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root,
                    request_id,
                    None,
                    false,
                )
                .await;
                break;
            }
            observation = async {
                tokio::select! {
                    _ = registry.terminal_transition_notified() => {}
                    _ = tokio::time::sleep(PENDING_POLL_INTERVAL) => {}
                }
                observe_deferred_bash_wait(
                    registry.clone(),
                    task_id.clone(),
                    session_id.clone(),
                    detach_on_user_message,
                    worker_session,
                    worker_cap_ms.map(|_| Duration::from_millis(crate::commands::bash_orchestrate::worker_wait_max_ms(&wait_ctx))),
                    remote_handback.map(|(at, _)| at),
                )
                .await
            } => {
                let DeferredWaitObservation { target_finished, detach_pending, remote_handback_due } = observation;
                let promotion_due = !block_to_completion && Instant::now() >= deadline;
                // While the module drains, every foreground wait (wait:true,
                // block_to_completion, or a plain wait window) is detached into
                // a background task so its request can answer before the drain
                // deadline. The command keeps running and its completion is
                // delivered like any promoted task's.
                let drain_detach_due = cancel.drain.is_active() && !server_completion;
                if let Some(progress) = &mut progress {
                    let _origin = crate::context::progress_origin_guard(&request_id, route.channel, route.epoch, corr);
                    progress.poll(target_finished || detach_pending || promotion_due || drain_detach_due || remote_handback_due, &mut |frame| wait_ctx.emit_progress(frame));
                }
                if !target_finished
                    && !detach_pending
                    && !promotion_due
                    && !drain_detach_due
                    && !remote_handback_due
                {
                    continue;
                }
                let (poll_control_tx, poll_control_rx) = oneshot::channel::<BashPollControl>();
                let (poll_text_tx, poll_text_rx) = oneshot::channel::<String>();
                let root_for_poll = root.clone();
                let request_id_for_poll = request_id.clone();
                let task_id_for_poll = task_id.clone();
                let session_for_poll = session_id.clone();
                let storage_for_poll = storage_dir.clone();
                let project_root_for_poll = project_root.clone();
                let format_context_for_poll = format_context.clone();
                let claim_for_poll = Arc::clone(&claim);

                let poll_rx = executor.submit_async(
                    root_for_poll,
                    Lane::PureRead,
                    request_id.clone(),
                    Box::new(move |ctx| {
                        log_ctx::with_session(Some(session_for_poll.clone()), || {
                            let mut poll_text_tx = Some(poll_text_tx);
                            let mut poll_control_tx = Some(poll_control_tx);

                            // Foreground polls only need task state. Terminal snapshots still
                            // return cached output.
                            let Some(snapshot) = crate::commands::bash_orchestrate::poll_bash_status(
                                ctx,
                                &task_id_for_poll,
                                &session_for_poll,
                                project_root_for_poll.as_deref(),
                                &storage_for_poll,
                                0,
                            ) else {
                                if claim_for_poll.answered_elsewhere() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
                                if detach_on_user_message {
                                    ctx.bash_background().end_wait_mode_session(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                } else {
                                    ctx.bash_background().unregister_foreground_task(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                }
                                return finish_bash_poll_done(
                                    crate::commands::bash_orchestrate::task_not_found_response(
                                        &request_id_for_poll,
                                        &task_id_for_poll,
                                    ),
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                    true,

                                );
                            };

                            if drain_detach_due && !snapshot.info.status.is_terminal() {
                                if claim_for_poll.answered_elsewhere() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
                                let response =
                                    crate::commands::bash_orchestrate::detach_bash_for_module_drain(
                                        ctx,
                                        &task_id_for_poll,
                                        &session_for_poll,
                                        &request_id_for_poll,
                                        worker_session,
                                        format_context_for_poll
                                            .bash_watch_available
                                            .unwrap_or(worker_session),
                                    );
                                if detach_on_user_message {
                                    ctx.bash_background().end_wait_mode_session(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                } else {
                                    ctx.bash_background().unregister_foreground_task(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                }
                                return finish_bash_poll_done(
                                    response,
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                    false,

                                );
                            }
                            if detach_on_user_message
                                && !snapshot.info.status.is_terminal()
                                && ctx
                                    .bash_background()
                                    .take_task_wait_mode_detach(&session_for_poll, &task_id_for_poll)
                            {
                                if claim_for_poll.answered_elsewhere() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
                                let response = crate::commands::bash_orchestrate::detach_wait_mode_bash(
                                    ctx,
                                    &task_id_for_poll,
                                    &session_for_poll,
                                    &request_id_for_poll,
                                    worker_session,
                                    format_context_for_poll
                                        .bash_watch_available
                                        .unwrap_or(worker_session),
                                );
                                ctx.bash_background().end_wait_mode_session(
                                    &session_for_poll,
                                    &task_id_for_poll,
                                );
                                return finish_bash_poll_done(
                                    response,
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                    false,

                                );
                            }
                            if remote_handback_due && !snapshot.info.status.is_terminal() {
                                // Reserve the reply only after hand-back formatting finishes,
                                // through the same delivery path as other ready results.
                                if claim_for_poll.answered_elsewhere() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
                                let response = crate::commands::bash_orchestrate::handback_remote_bash(
                                    ctx,
                                    &task_id_for_poll,
                                    &session_for_poll,
                                    &request_id_for_poll,
                                    remote_handback.map_or(0, |(_, waited_ms)| waited_ms),
                                    worker_session,
                                    format_context_for_poll
                                        .bash_watch_available
                                        .unwrap_or(worker_session),
                                );
                                if detach_on_user_message {
                                    ctx.bash_background().end_wait_mode_session(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                } else {
                                    ctx.bash_background().unregister_foreground_task(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                }
                                return finish_bash_poll_done(
                                    response,
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                    false,
                                );
                            }
                            match crate::commands::bash_orchestrate::decide_bash_step(
                                snapshot,
                                deadline,
                                block_to_completion,
                                worker_cap_ms.is_some()
                                    && crate::commands::bash_orchestrate::worker_kill_deadline_within_handoff_margin(
                                        ctx.bash_background().hard_kill_remaining(
                                            &task_id_for_poll,
                                            &session_for_poll,
                                        ),
                                    ),
                                Instant::now(),
                                &request_id_for_poll,
                            ) {
                                crate::commands::bash_orchestrate::BashStep::Done(response) => {
                                    if claim_for_poll.answered_elsewhere() {
                                        return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                    }
                                    if detach_on_user_message {
                                        ctx.bash_background().end_wait_mode_session(
                                            &session_for_poll,
                                            &task_id_for_poll,
                                        );
                                    } else {
                                        ctx.bash_background().unregister_foreground_task(
                                            &session_for_poll,
                                            &task_id_for_poll,
                                        );
                                    }
                                    finish_bash_poll_done(
                                        response,
                                        ctx,
                                        &session_for_poll,
                                        &format_context_for_poll,
                                        &mut poll_text_tx,
                                        &mut poll_control_tx,
                                        true,

                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Promote => {
                                    // Moving the command to background may write task
                                    // metadata. Do not reserve the reply until that work
                                    // and status formatting finish; the timer can answer meanwhile.
                                    if let Some(tx) = poll_control_tx.take() {
                                        let _ = tx.send(BashPollControl::Promote);
                                    }
                                    Response::success(
                                        request_id_for_poll,
                                        json!({ "subc_bash_step": "promote" }),
                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Wait => {
                                    if let Some(tx) = poll_control_tx.take() {
                                        let _ = tx.send(BashPollControl::Wait);
                                    }
                                    Response::success(
                                        request_id_for_poll,
                                        json!({ "subc_bash_step": "wait" }),
                                    )
                                }
                            }
                        })
                    }),
                );
                // The poll may sit queued behind other work on the root's actor;
                // if the module loop answers the call meanwhile, stop waiting.
                let poll_response = tokio::select! {
                    response = await_executor_response(poll_rx, request_id.clone()) => response,
                    _ = claim.module_loop_claimed() => {
                        send_bash_deferred_completion(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            None,
                            false,
                        )
                        .await;
                        break;
                    }
                };
                let _ = send_counted_channel(
                    &poll_touch_tx,
                    &metrics.bash_poll_touch_queued,
                    root.clone(),
                )
                .await;
                match poll_control_rx.await.unwrap_or(BashPollControl::Done) {
                    BashPollControl::Done => {
                        let text = poll_text_rx.await.unwrap_or_else(|_| {
                            crate::subc_format::format_response_with_context(
                                "bash",
                                &poll_response,
                                &format_context,
                            )
                        });
                        let result = ToolCallResult {
                            text,
                            response: poll_response,
                        };
                        send_ready_bash_result(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            result,
                            &wait_ctx,
                            &format_context,
                            repeat.clone(),
                            &claim,
                            &task_id,
                            &session_id,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Abandoned => {
                        if claim.claimed_by_deadline() {
                            break;
                        }
                        send_bash_deferred_completion(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            None,
                            false,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Promote => {
                        let result = submit_bash_promote(
                            &executor,
                            root.clone(),
                            request_id.clone(),
                            task_id.clone(),
                            session_id.clone(),
                            timeout,
                            wait_window_ms,
                            worker_session,
                            worker_cap_ms.is_some(),
                            format_context.clone(),
                            detach_on_user_message,
                        )
                        .await;
                        send_ready_bash_result(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            result,
                            &wait_ctx,
                            &format_context,
                            repeat.clone(),
                            &claim,
                            &task_id,
                            &session_id,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Wait => {}
                }
            }
        }
    }
}

async fn submit_bash_promote(
    executor: &Arc<Executor>,
    root: ProjectRootId,
    request_id: String,
    task_id: String,
    session_id: String,
    timeout: Option<u64>,
    wait_window_ms: u64,
    worker_session: bool,
    capped_worker_wait: bool,
    format_context: crate::subc_format::FormatContext,
    detach_on_user_message: bool,
) -> ToolCallResult {
    let (text_tx, text_rx) = oneshot::channel::<String>();
    let request_id_for_promote = request_id.clone();
    let task_id_for_promote = task_id.clone();
    let session_for_promote = session_id.clone();
    let format_context_for_promote = format_context.clone();
    let promote_rx = executor.submit_async(
        root,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            release_wait_registration(
                ctx.bash_background(),
                &session_for_promote,
                &task_id_for_promote,
                detach_on_user_message,
            );
            log_ctx::with_session(Some(session_for_promote.clone()), || {
                #[cfg(any(test, feature = "test-timing-hooks"))]
                if let Ok(path) = std::env::var("AFT_TEST_SUBC_BASH_PROMOTE_STARTED_FILE") {
                    std::fs::write(path, b"started").expect("write promotion start marker");
                }
                #[cfg(any(test, feature = "test-timing-hooks"))]
                if let Some(delay) = std::env::var("AFT_TEST_SUBC_BASH_PROMOTE_DELAY_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    std::thread::sleep(Duration::from_millis(delay));
                }
                #[cfg(any(test, feature = "test-timing-hooks"))]
                let forced = std::env::var_os("AFT_TEST_FORCE_SUBC_BASH_PROMOTE_ERROR");
                #[cfg(not(any(test, feature = "test-timing-hooks")))]
                let forced: Option<std::ffi::OsString> = None;
                let response = if let Some(value) = forced {
                    if value.to_string_lossy() == "panic" {
                        panic!("forced subc bash promote panic");
                    }
                    Response::error(
                        &request_id_for_promote,
                        "execution_failed",
                        "forced subc bash promote failure",
                    )
                } else {
                    crate::commands::bash_orchestrate::promote_bash(
                        ctx,
                        &task_id_for_promote,
                        &session_for_promote,
                        timeout,
                        wait_window_ms,
                        &request_id_for_promote,
                        worker_session,
                        format_context_for_promote
                            .bash_watch_available
                            .unwrap_or(worker_session),
                        capped_worker_wait,
                    )
                };
                let result = finalized_bash_result(
                    response,
                    ctx,
                    &session_for_promote,
                    &format_context_for_promote,
                    false,
                    None,
                );
                let ToolCallResult { text, response } = result;
                let _ = text_tx.send(text);
                response
            })
        }),
    );
    let response = await_executor_response(promote_rx, request_id).await;
    let text = text_rx.await.unwrap_or_else(|_| {
        crate::subc_format::format_response_with_context("bash", &response, &format_context)
    });
    ToolCallResult { text, response }
}

/// Rechecks a finalized reply against the task's saved input-capture state
/// just before it is sent. A live-task reply for a task whose runner never sent
/// Accepted gets the capture warning; if that task has since finished, the
/// reply is replaced by its saved outcome (see [`carry_finalization`]).
async fn recheck_runon_capture(
    result: ToolCallResult,
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
) -> ToolCallResult {
    let announced_live =
        crate::commands::bash_orchestrate::reply_announces_live_task(&result.response);
    let replacement = warn_if_runon_capture_missing(
        ctx.bash_background().clone(),
        task_id.to_owned(),
        session_id.to_owned(),
        copy_response(&result.response),
    )
    .await;
    if announced_live && !crate::commands::bash_orchestrate::reply_announces_live_task(&replacement)
    {
        return carry_finalization(result, replacement, ctx, session_id, format_context).await;
    }
    let mut result = ToolCallResult {
        text: result.text,
        response: replacement,
    };
    if result.response.data["output"]
        .as_str()
        .is_some_and(|text| text.ends_with(REMOTE_INPUT_CAPTURE_WARNING))
        && !result.text.contains(REMOTE_INPUT_CAPTURE_WARNING)
    {
        result
            .text
            .push_str(&format!("\n{REMOTE_INPUT_CAPTURE_WARNING}"));
    }
    result
}

/// Builds the reply for a task that finished before its runner confirmed input
/// capture, replacing a live-task reply that was already finalized.
///
/// Finalization is not run again: it consumed one-shot items into the earlier
/// reply (background completions drained for the session, cancelled-call
/// notices) and appended the status bar, so a second pass would lose them.
/// Instead, everything the finalizer added after the formatted body, and the
/// completions and gaps it attached to the response, move onto the replacement.
/// The formatted body stays a prefix of the text, which the repeat breaker in
/// `observe_completed_bash_result` relies on. A finished result is acknowledged
/// for the session, as finalization does for a finished reply, so the same
/// outcome is not delivered again as a background completion.
async fn carry_finalization(
    previous: ToolCallResult,
    replacement: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
) -> ToolCallResult {
    let previous_body = crate::subc_format::format_response_with_context(
        "bash",
        &previous.response,
        format_context,
    );
    let additions = previous
        .text
        .strip_prefix(&previous_body)
        .unwrap_or_default()
        .trim_start_matches('\n');
    let mut response = replacement;
    if let Some(completions) = previous.response.data.get("bg_completions") {
        response.data["bg_completions"] = completions.clone();
    }
    if previous.response.data.get("complete") == Some(&json!(false)) {
        response.data["complete"] = json!(false);
    }
    if let Some(previous_gaps) = previous.response.data["gaps"].as_array() {
        if !response.data["gaps"].is_array() {
            response.data["gaps"] = json!([]);
        }
        if let Some(gaps) = response.data["gaps"].as_array_mut() {
            for gap in previous_gaps {
                if !gaps.contains(gap) {
                    gaps.push(gap.clone());
                }
            }
        }
    }
    let mut text =
        crate::subc_format::format_response_with_context("bash", &response, format_context);
    if !additions.is_empty() {
        if !text.is_empty() {
            text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
        }
        text.push_str(additions);
    }
    let finished = response.success
        && matches!(
            response.data["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out" | "fate_unknown")
        );
    if let Some(task_id) = response.data["task_id"].as_str().filter(|_| finished) {
        let registry = ctx.bash_background().clone();
        let task_id = task_id.to_owned();
        let session_id = session_id.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            registry.ack_terminal_result_for_session(&task_id, &session_id)
        })
        .await;
    }
    ToolCallResult { text, response }
}

#[allow(clippy::too_many_arguments)]
async fn send_ready_bash_result(
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    metrics: &DispatchPathMetrics,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: ToolCallResult,
    ctx: &AppContext,
    format_context: &crate::subc_format::FormatContext,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
    claim: &drain::BashCallClaim,
    task_id: &str,
    session_id: &str,
) {
    // Capture is not a second blocking-call deadline. Inspect it only when the
    // normal wait is ready to answer, and preserve that answer with a warning.
    let result = recheck_runon_capture(result, ctx, task_id, session_id, format_context).await;
    let fatal = response_is_fatal_panic(&result.response);
    if claim.try_claim_for_wait_task() {
        let result = observe_completed_bash_result(result, ctx, format_context, repeat);
        send_bash_deferred_completion(
            completion_tx,
            metrics,
            route,
            corr,
            flags,
            ver,
            root,
            request_id,
            Some(result),
            fatal,
        )
        .await;
    } else if claim.claimed_by_deadline() && !result.response.success && !fatal {
        // The deadline reply already exposed the task id. Keep a discarded
        // nonfatal executor error visible without answering the request again.
        let error = result
            .response
            .data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(&result.text);
        // Runs on the shared subc runtime; tag with the task's session.
        crate::log_ctx::with_session(Some(session_id.to_string()), || {
            crate::slog_warn!(
                "subc attach: late bash result discarded after handoff task_id={task_id:?} session={session_id:?} error={error:?}"
            );
        });
    } else if claim.claimed_by_module_loop() {
        send_bash_deferred_completion(
            completion_tx,
            metrics,
            route,
            corr,
            flags,
            ver,
            root,
            request_id,
            None,
            fatal,
        )
        .await;
    } else if claim.claimed_by_deadline() && fatal {
        // The deadline already sent a task id and decremented this request's
        // root and route wait counts. A later executor panic must restart the
        // module without sending another tool response or decrementing again.
        let _ = send_counted_channel(
            completion_tx,
            &metrics.bash_deferred_queued,
            BashDeferredCompletion {
                route,
                corr,
                flags,
                ver,
                root,
                request_id,
                result: None,
                fatal: true,
                settles_hold: false,
            },
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_bash_deferred_completion(
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    metrics: &DispatchPathMetrics,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: Option<ToolCallResult>,
    fatal: bool,
) {
    let _ = send_counted_channel(
        completion_tx,
        &metrics.bash_deferred_queued,
        BashDeferredCompletion {
            route,
            corr,
            flags,
            ver,
            root,
            request_id,
            result,
            fatal,
            settles_hold: true,
        },
    )
    .await;
}

pub(super) async fn handle_bash_deferred_completion(
    tx: &WriterSender,
    done: BashDeferredCompletion,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let route_id = done.route;
    if done.settles_hold {
        metrics.held_bash_calls.remove(done.route, done.corr);
        if let Some(meta) = live_roots.get_mut(&done.root) {
            meta.active_bash_waits = meta.active_bash_waits.saturating_sub(1);
            meta.note_activity();
        }
        let remove_route_cancel = if let Some(cancel) = route_bash_cancels.get_mut(&route_id) {
            cancel.active_waits = cancel.active_waits.saturating_sub(1);
            cancel.active_waits == 0
        } else {
            false
        };
        if remove_route_cancel {
            route_bash_cancels.remove(&route_id);
        }
    }

    if let Some(result) = done.result {
        if let Some(identity) = routes.get(&route_id) {
            let frame = build_tool_response_frame(
                done.ver,
                done.route,
                done.corr,
                done.flags,
                &result,
                identity.reply_policy(),
            )?;
            send_reliable_writer_frame(tx, metrics, frame, "deferred bash response").await?;
        } else {
            log::debug!(
                "subc attach: dropping deferred bash response {} for unbound route {}",
                done.request_id,
                done.route
            );
        }
    } else {
        log::debug!(
            "subc attach: deferred bash wait {} cancelled before delivery on route {}",
            done.request_id,
            done.route
        );
    }

    if done.fatal {
        signal_fatal_teardown(tx, Some(done.route), done.ver, done.corr, shutdown, metrics).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn bash_denied_untrusted_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    format_context: crate::subc_format::FormatContext,
) -> BashDeferredCompletion {
    let response = bash_denied_untrusted_response(request_id.clone());
    BashDeferredCompletion {
        route,
        corr,
        flags,
        ver,
        root,
        request_id,
        result: Some(bash_result_from_response(response, &format_context)),
        fatal: false,
        settles_hold: true,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn bash_authorization_failed_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    format_context: crate::subc_format::FormatContext,
) -> BashDeferredCompletion {
    let response = Response::error(
        &request_id,
        "database_unavailable",
        super::tool_provider::AUTHORIZATION_FAILURE_MESSAGE,
    );
    BashDeferredCompletion {
        route,
        corr,
        flags,
        ver,
        root,
        request_id,
        result: Some(bash_result_from_response(response, &format_context)),
        fatal: false,
        settles_hold: true,
    }
}

/// Terminal for a bash call answered early because the module is draining. The
/// command never ran, so the retryable error is safe to retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn bash_module_draining_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    format_context: crate::subc_format::FormatContext,
) -> BashDeferredCompletion {
    let response = drain::module_draining_response(&request_id, "bash");
    BashDeferredCompletion {
        route,
        corr,
        flags,
        ver,
        root,
        request_id,
        result: Some(bash_result_from_response(response, &format_context)),
        fatal: false,
        settles_hold: true,
    }
}

/// Decrements the root's and the route's active bash wait counts that a held
/// bash call took when it filed its permission question, without answering
/// the call: `tool.withdraw` settled it, and the caller sends its
/// `not_started` terminal frame.
pub(super) fn bash_withdrawn_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
) -> BashDeferredCompletion {
    BashDeferredCompletion {
        route,
        corr,
        flags,
        ver,
        root,
        request_id,
        result: None,
        fatal: false,
        settles_hold: true,
    }
}

pub(super) fn bash_denied_untrusted_response(request_id: impl Into<String>) -> Response {
    Response::error(
        request_id.into(),
        "bash_denied_untrusted",
        "remote/MCP-facade binds cannot run shell commands",
    )
}

// These gates belong to one call, not the process: parallel tests cannot move
// another startup's clock or release its deadline.
#[cfg(test)]
#[derive(Default)]
struct BashTestDeadline {
    armed: tokio::sync::Notify,
    fire: tokio::sync::Notify,
}

#[cfg(test)]
impl BashTestDeadline {
    async fn wait(&self) {
        self.armed.notify_one();
        self.fire.notified().await;
    }
}

#[cfg(test)]
struct BashTimingForTest {
    commit_time: Instant,
    capture_wait: Option<Duration>,
    capture_registered: tokio::sync::Notify,
    startup: BashTestDeadline,
    inline_settle: BashTestDeadline,
    reply: BashTestDeadline,
    reply_uses_clock: bool,
    committed: tokio::sync::Notify,
    task_id: std::sync::Mutex<Option<String>>,
    admission: (std::sync::Mutex<bool>, std::sync::Condvar),
    result: (std::sync::Mutex<bool>, std::sync::Condvar),
    dispatch: Option<DispatchFn>,
    checkpoint: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}

#[cfg(test)]
thread_local! {
    static BASH_TIMING_FOR_TEST: std::cell::RefCell<Option<Arc<BashTimingForTest>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_bash_timing_for_test<T>(
    timing: Option<Arc<BashTimingForTest>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Arc<BashTimingForTest>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BASH_TIMING_FOR_TEST.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let commit_time = timing.as_ref().map(|timing| timing.commit_time);
    let _restore = Restore(BASH_TIMING_FOR_TEST.with(|slot| slot.replace(timing)));
    match commit_time {
        Some(now) => crate::bash_background::with_spawn_commit_time_for_test(now, run),
        None => run(),
    }
}

#[cfg(all(test, unix))]
pub(super) async fn assert_deferred_wait_entrypoint_for_test(
    command: &str,
    release: &std::path::Path,
) {
    grant_path_tests::assert_deferred_wait_entrypoint(command, Some(release)).await;
}

#[cfg(test)]
mod grant_path_tests {
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_progress_reaches_subc_origin_before_command_finishes() {
        use super::*;
        let (dir, root) = super::super::test_support::test_root("bash-progress");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.storage_dir = Some(dir.path().join("storage"));
        });
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            if let crate::protocol::PushFrame::Progress(frame) = frame {
                let _ = progress_tx.send(frame);
            }
        }))));
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (tx, mut rx) = mpsc::channel(8);
        let (touch, _touch_rx) = mpsc::channel(8);
        submit_deferred_bash(
            true,
            &executor,
            &tx,
            &touch,
            &metrics,
            |req, ctx| crate::commands::bash::handle(&req, ctx),
            root,
            dir.path().to_path_buf(),
            "progress-session".into(),
            "progress-request".into(),
            RouteChannel {
                channel: 7,
                epoch: 3,
            },
            1052,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            json!({"command": "echo hello; while [ ! -f release ]; do sleep 0.1; done", "wait": true}),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            super::remote_policy::RemoteSource::None,
            Instant::now(),
        );
        let frame = tokio::time::timeout(Duration::from_secs(15), progress_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.request_id, "progress-request");
        assert_eq!(frame.chunk, "hello\n");
        let origin = frame.origin.unwrap();
        assert_eq!((origin.channel, origin.epoch, origin.corr), (7, 3, 1052));
        assert!(
            rx.try_recv().is_err(),
            "terminal reply arrived before releasing the child"
        );
        std::fs::write(dir.path().join("release"), "").unwrap();
        let done = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(done.response_for_test().success);
    }
    #[test]
    fn finalized_terminal_bash_result_carries_the_stored_record_on_tool_result_routes() {
        let manifest = crate::db::rich_ui::test_view_manifest_for_test();
        let pin = manifest.pins[0].clone();
        let _writing = crate::db::rich_ui::enable_test_view_writing_for_test(manifest.clone());
        let (dir, _root) = super::super::test_support::test_root("finalized-view");
        let ctx = super::super::test_support::test_ctx();
        let db = std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(std::sync::Arc::clone(&db));
        ctx.bash_background()
            .set_db_pool(std::sync::Arc::clone(&db));
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Opencode);
        let session = "finalized-session";
        let task = "bash-0000000000000041";
        let mut metadata = crate::db::rich_ui::terminal_task_metadata_for_test(Some(&pin));
        metadata.session_id = session.into();
        metadata.task_id = task.into();
        metadata.harness = Some("opencode".into());
        let row = crate::db::rich_ui::task_row_for_test(
            &metadata,
            "opencode",
            "completed",
            Some(7),
            true,
        );
        let stored = {
            let conn = db.lock().unwrap();
            crate::db::rich_ui::upsert_task_row_with_view(&conn, &row, &metadata).unwrap();
            crate::db::rich_ui::view_record_json(&conn, "opencode", session, task)
                .unwrap()
                .expect("record must be stored")
        };
        let body = || {
            serde_json::json!({
                "task_id": task,
                "status": "completed",
                "exit_code": 0,
                "output": "done",
                "output_preview": "done",
                "output_truncated": false,
            })
        };
        // A legacy-envelope route never receives the record.
        let legacy = finalized_bash_result(
            Response::success("r1", body()),
            &ctx,
            session,
            &crate::subc_format::FormatContext::default(),
            false,
            None,
        );
        assert!(legacy.response.data.get("view").is_none());
        // A tool-result/v1 route receives the stored record, with the same
        // model-visible text as the legacy reply.
        let _v1 = super::result_body::enter(true);
        let done = finalized_bash_result(
            Response::success("r1", body()),
            &ctx,
            session,
            &crate::subc_format::FormatContext::default(),
            false,
            None,
        );
        assert_eq!(done.response.data["view"], stored);
        assert_eq!(done.text, legacy.text);
    }

    /// The deferred bash dispatch installs the call's schema pin around the
    /// spawn: a finished task persists that pin on its record, and the
    /// terminal upsert stores a view record carrying it.
    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_bash_dispatch_records_the_call_schema_pin() {
        let manifest = crate::db::rich_ui::test_view_manifest_for_test();
        let pin = manifest.pins[0].clone();
        let _writing = crate::db::rich_ui::enable_test_view_writing_for_test(manifest);
        let (dir, root) = super::super::test_support::test_root("dispatch-schema-pin");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.storage_dir = Some(dir.path().join("storage"));
        });
        let db = std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(std::sync::Arc::clone(&db));
        ctx.bash_background()
            .set_db_pool(std::sync::Arc::clone(&db));
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Opencode);
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (tx, mut rx) = mpsc::channel(8);
        let (touch, _touch_rx) = mpsc::channel(8);
        let session = "pin-dispatch-session";
        submit_deferred_bash(
            false,
            &executor,
            &tx,
            &touch,
            &metrics,
            crate::bash_background::completed_spawn_for_test,
            root.clone(),
            dir.path().to_path_buf(),
            session.into(),
            "pin-dispatch".into(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            json!({"command": "exit 0", "background": true}),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                run_scope: None,
                speech_seed: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            Some(pin.clone()),
            None,
            None,
            false,
            false,
            false,
            false,
            super::remote_policy::RemoteSource::None,
            Instant::now(),
        );
        let done = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap().to_string();
        let started = Instant::now();
        let stored = loop {
            let conn = db.lock().unwrap();
            let row =
                crate::db::bash_tasks::get_bash_task(&conn, "opencode", session, &task_id).unwrap();
            let settled = row.as_ref().is_some_and(|row| row.status == "completed");
            let record =
                crate::db::rich_ui::view_record_json(&conn, "opencode", session, &task_id).unwrap();
            drop(conn);
            if settled
                && record
                    .as_ref()
                    .and_then(|record| record.get("schema_pin"))
                    .and_then(|value| value.as_str())
                    == Some(pin.as_str())
            {
                break (row.unwrap(), record.unwrap());
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "pinned task did not settle with a view record"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let persisted: serde_json::Value = serde_json::from_str(&stored.0.metadata).unwrap();
        assert_eq!(persisted["schema_pin"], pin);
        assert_eq!(stored.1["schema_pin"], pin);
        assert_eq!(stored.1["task_id"], task_id);
    }

    fn secret_bash_scope_probe(req: RawRequest, _: &AppContext) -> Response {
        use crate::secret::{Material, MaterialError, Resolution, SecretScope, SecretVault};
        struct Vault(std::sync::atomic::AtomicUsize);
        impl SecretVault for Vault {
            fn resolve(&self, scope: &SecretScope, _: &str) -> Resolution {
                assert_eq!(scope.reference(), "secret-bash");
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Resolution::Unavailable
            }
            fn material(
                &self,
                _: &SecretScope,
                _: &str,
                _: &str,
            ) -> Result<Material, MaterialError> {
                panic!("launch must not request material")
            }
            fn audit(
                &self,
                _: &secret_rules::audit::AuditRecordV1,
            ) -> Result<(), crate::secret::AuditError> {
                Ok(())
            }
        }
        let vault = Vault(std::sync::atomic::AtomicUsize::new(0));
        let decision = super::super::secret_consent::resolve_current_secret_label(&vault, "test");
        let super::super::secret_consent::LaunchDecision::Refuse(outcome) = decision else {
            panic!("unavailable vault must refuse");
        };
        Response::error_with_data(
            &req.id,
            outcome.code().unwrap(),
            "scope probe",
            json!({"resolve_calls": vault.0.load(std::sync::atomic::Ordering::SeqCst)}),
        )
    }

    #[tokio::test]
    async fn secret_bash_dedicated_executor_keeps_scope_on_resume_and_clears_it() {
        let (dir, root) = super::super::test_support::test_root("secret-bash-dedicated");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.storage_dir = Some(dir.path().join("storage"));
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let stamp: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"secret-bash","kind":"worker","scope_epoch":1,"owner_authorized":true})).unwrap();
        for (stamped, resumed) in [(true, false), (true, true), (false, false)] {
            let (tx, mut rx) = mpsc::channel(8);
            let (touch, _touch_rx) = mpsc::channel(8);
            let submit = || {
                submit_deferred_bash(
                    false,
                    &executor,
                    &tx,
                    &touch,
                    &metrics,
                    secret_bash_scope_probe,
                    root.clone(),
                    root.as_path().into(),
                    "secret-bash".into(),
                    "secret-bash".into(),
                    RouteChannel {
                        channel: 1,
                        epoch: 1,
                    },
                    1,
                    Flags::new(false, Priority::Interactive, false),
                    PROTOCOL_VERSION,
                    json!({"command":"echo '{{ck-secret:test}}'", "scope":{"ref":"forged-head"}}),
                    crate::subc_format::FormatContext::default(),
                    BashWaitCancel {
                        speech_seed: None,
                        run_scope: None,
                        scope: stamped.then(|| stamp.clone()),
                        late_results: false,
                        ledger_carrier: None,
                        connection: PersistentCancelSignal::new(),
                        route: PersistentCancelSignal::new(),
                        drain: drain::ModuleDrainWindow::default(),
                    },
                    BindTrust::FirstParty,
                    crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                    None,
                    None,
                    // The secret probe carries no v1 schema pin.
                    None,
                    resumed.then(|| vec![]),
                    None,
                    false,
                    false,
                    false,
                    false,
                    super::remote_policy::RemoteSource::None,
                    Instant::now(),
                )
            };
            if stamped {
                crate::subc::with_secret_identity_for_test(root.as_path(), stamp.clone(), submit);
            } else {
                submit();
            }
            let done = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let response = done.response_for_test();
            assert_eq!(
                response.data["code"],
                if stamped {
                    "secrets_unavailable"
                } else {
                    "secret_refused"
                }
            );
            assert_eq!(response.data["resolve_calls"], usize::from(stamped));
            assert!(matches!(
                super::super::secret_consent::current_call_scopes(),
                Err(crate::secret::Ineligibility::NoScope)
            ));
        }
    }
    #[cfg(unix)]
    static CHANGED_FILES_BUDGET_FOR_TEST: std::sync::Mutex<
        Option<crate::bash_changed_files::SharedCallDeadline>,
    > = std::sync::Mutex::new(None);

    #[cfg(unix)]
    fn changed_files_budget_running_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let deadline = crate::bash_background::changed_files_deadline_for_spawn()
            .expect("reply deadline bound before dispatch");
        assert!(deadline.lock().unwrap().is_some());
        *CHANGED_FILES_BUDGET_FOR_TEST.lock().unwrap() = Some(deadline);
        slow_running_write_stub(req, ctx)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_files_budget_clears_at_startup_handoff_before_spawn_finishes() {
        let received_at = Instant::now();
        let (dir, root) = super::super::test_support::test_root("bash-changed-files-budget");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 500;
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
            config.experimental_bash_rewrite = false;
            config.bash.changed_files = crate::config::BashChangedFiles::List;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut timing = BashTimingForTest::new(received_at);
        Arc::get_mut(&mut timing).unwrap().dispatch = Some(changed_files_budget_running_stub);
        let rx = with_bash_timing_for_test(Some(timing.clone()), || {
            deadline_test_call_owned(
                &executor,
                &root,
                controlled_startup_dispatch,
                json!({"command":"printf ran > started-command", "timeout":60000}),
                received_at,
                false,
            )
        });
        let mut startup = ControlledStartup {
            call: StartupCall { dir, ctx, rx },
            timing,
        };
        let done = startup.reply_at_checkpoint().await;
        assert!(
            done.response_for_test().success,
            "{:?}",
            done.response_for_test()
        );
        let state = CHANGED_FILES_BUDGET_FOR_TEST
            .lock()
            .unwrap()
            .take()
            .expect("capture budget observed");
        assert!(
            state.lock().unwrap().is_none(),
            "handoff releases the reply budget before the executor finishes"
        );
        startup.finish().await;
    }
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use serde_json::json;
    use tokio::sync::{mpsc, oneshot};

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn quick_terminal_spawn_replies_are_complete() {
        for (background, pty, server_completion) in [
            (true, false, false),
            (false, true, false),
            (false, false, false),
            (true, false, true),
        ] {
            let (dir, root) = super::super::test_support::test_root("quick-terminal-subc");
            let ctx = super::super::test_support::test_ctx();
            ctx.update_config(|config| {
                config.project_root = Some(dir.path().into());
                config.storage_dir = Some(dir.path().join("storage"));
                config.sandbox.profile.head = crate::config::SandboxMode::Off;
            });
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), ctx);
            let metrics = Arc::new(DispatchPathMetrics::new());
            let (tx, mut rx) = mpsc::channel(8);
            let (touch, _touch_rx) = mpsc::channel(8);
            submit_deferred_bash(
                false,
                &executor,
                &tx,
                &touch,
                &metrics,
                crate::bash_background::completed_spawn_for_test,
                root.clone(),
                root.as_path().into(),
                "quick-terminal-session".into(),
                "quick-terminal".into(),
                RouteChannel {
                    channel: 1,
                    epoch: 1,
                },
                1,
                Flags::new(false, Priority::Interactive, false),
                PROTOCOL_VERSION,
                json!({"command": "printf 'quick-output\\n'; exit 7", "background": background, "pty": pty}),
                crate::subc_format::FormatContext::default(),
                BashWaitCancel {
                    speech_seed: None,
                    run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(),
                },
                BindTrust::FirstParty,
                crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                None,
                None,
                None,
                None,
                None,
                false,
                false,
                false,
                server_completion,
                super::remote_policy::RemoteSource::None,
                Instant::now(),
            );
            let done = tokio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let response = done.response_for_test();
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["status"], "failed");
            let task_id = response.data["task_id"].as_str().unwrap();
            let text = done.text_for_test().unwrap();
            assert!(text.contains(task_id), "reply must name its task: {text:?}");
            if (background || pty) && !server_completion {
                assert!(
                    text.contains(
                        &crate::commands::bash_orchestrate::format_background_launch(
                            task_id, pty, false, false
                        )
                    ),
                    "launch must include task guidance: {text}"
                );
            } else {
                assert!(text.contains("quick-output"), "{text}");
                assert!(text.contains("[exit code: 7]"), "{text}");
                assert_eq!(response.data["exit_code"], 7);
            }
        }
    }

    #[test]
    fn late_nonfatal_bash_error_after_handoff_logs_identity_without_another_reply() {
        let (_dir, root) = super::super::test_support::test_root("late-nonfatal-log");
        let ctx = super::super::test_support::test_ctx();
        let claim = drain::BashCallClaim::default();
        assert!(claim.claim_for_deadline_handoff());
        let (tx, mut rx) = mpsc::channel(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_, lines) = crate::logging::capture_log_lines(|| {
            runtime.block_on(send_ready_bash_result(
                &tx,
                &DispatchPathMetrics::new(),
                RouteChannel {
                    channel: 1,
                    epoch: 1,
                },
                116,
                Flags::new(false, Priority::Interactive, false),
                PROTOCOL_VERSION,
                root,
                "late-error".into(),
                ToolCallResult {
                    text: "late error".into(),
                    response: Response::error(
                        "late-error",
                        "execution_failed",
                        "late failure\nsecond line",
                    ),
                },
                &ctx,
                &crate::subc_format::FormatContext::default(),
                None,
                &claim,
                "late-task",
                "late-session",
            ))
        });
        assert_eq!(lines.len(), 1, "one canonical warning: {lines:?}");
        let line = &lines[0];
        assert!(
            line.contains("late bash result discarded after handoff"),
            "{line}"
        );
        assert!(line.contains("task_id=\"late-task\""), "{line}");
        assert!(line.contains("session=\"late-session\""), "{line}");
        assert!(
            line.contains("error=\"late failure\\nsecond line\""),
            "{line}"
        );
        assert!(!line.contains('\n'), "warning must fit one line: {line:?}");
        assert!(
            rx.try_recv().is_err(),
            "handoff already answered and settled the request"
        );
    }

    #[test]
    fn failed_background_handoff_logs_identity_and_keeps_status_observable() {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};

        let (dir, _root) = super::super::test_support::test_root("detach-failure-log");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let marker = dir.path().join("started");
        let release = dir.path().join("release");
        let _release_guard = DeferredWaitReleaseGuard(vec![release.clone()]);
        let marker_path = marker.to_string_lossy().replace('\\', "/");
        let release_path = release.to_string_lossy().replace('\\', "/");
        let command = if cfg!(windows) {
            format!("Set-Content -Path '{marker_path}' -Value started; while (-not (Test-Path '{release_path}')) {{ Start-Sleep -Milliseconds 20 }}; Write-Output 'handoff-finished'")
        } else {
            format!("printf started > '{marker_path}'; until [ -e '{release_path}' ]; do sleep 0.02; done; printf 'handoff-finished\\n'")
        };
        let session = "detach-log-session";
        let request = RawRequest {
            id: "detach-log".into(),
            command: "bash".into(),
            lsp_hints: None,
            session_id: Some(session.into()),
            params: json!({ "command": command, "timeout": 60000 }),
        };
        let spawned = running_bash_stub(request, &ctx);
        assert!(spawned.success, "{spawned:?}");
        let task_id = spawned.data["task_id"].as_str().unwrap().to_string();
        let started_by = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                Instant::now() < started_by,
                "foreground command must actually start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let registry = ctx.bash_background();
        registry.register_foreground_task(session, &task_id);
        let target = drain::BashDetachTarget {
            reply_cache: registry.drain_reply_cache(&task_id, session),
            run_scoped: false,
            task_id: task_id.clone(),
            session_id: session.into(),
            wait_mode: false,
            worker_session: false,
            server_completion: false,
            late_results: false,
            registry: registry.clone(),
            request_id: "detach-log".into(),
            ver: PROTOCOL_VERSION,
            flags: Flags::new(false, Priority::Interactive, false),
            format_context: crate::subc_format::FormatContext::default(),
        };
        let (_, lines) = crate::logging::capture_log_lines(|| {
            with_task_io_fault(TaskIoFault::RunningEnospc, || {
                detach_held_bash_sync(target, false)
            })
        });
        let warnings: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("bash background handoff failed"))
            .collect();
        assert_eq!(warnings.len(), 1, "one handoff warning: {lines:?}");
        let line = warnings[0];
        assert!(line.contains(&format!("task_id={task_id:?}")), "{line}");
        assert!(line.contains("session=\"detach-log-session\""), "{line}");
        assert!(
            line.contains("error=\"failed to promote background task:"),
            "{line}"
        );
        assert!(!line.contains('\n'), "warning must fit one line: {line:?}");

        // A failed metadata write must not destroy the task id that the caller
        // already received. Exercise the same status handler the caller can use.
        let status_request = RawRequest {
            id: "status-after-detach-error".into(),
            command: "bash_status".into(),
            lsp_hints: None,
            session_id: Some(session.into()),
            params: json!({ "task_id": task_id }),
        };
        let running = crate::commands::bash_status::handle(&status_request, &ctx);
        assert!(running.success, "{running:?}");
        assert_eq!(running.data["status"], "running", "{running:?}");
        std::fs::write(&release, b"release").unwrap();
        let completed_by = Instant::now() + Duration::from_secs(10);
        let completed = loop {
            let status = crate::commands::bash_status::handle(&status_request, &ctx);
            assert!(status.success, "{status:?}");
            if status.data["status"] == "completed" {
                break status;
            }
            assert!(
                Instant::now() < completed_by,
                "command must finish after release: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(completed.data["exit_code"], 0, "{completed:?}");
        let task = registry.task_for_test(&task_id).unwrap();
        let state = task.state.lock().unwrap();
        let notify = state.metadata.notify_on_completion;
        let delivered = state.metadata.completion_delivered;
        drop(state);
        let notices = registry.drain_completions().len();
        eprintln!("failed-detach observation: running_status={} terminal_status={} exit_code={} notify_on_completion={notify} completion_delivered={delivered} completion_notices={notices}",
            running.data["status"], completed.data["status"], completed.data["exit_code"]);
    }

    #[tokio::test]
    async fn late_promotion_fatal_signals_teardown_without_settling_hold_twice() {
        let (_dir, root) = super::super::test_support::test_root("late-promotion-fatal");
        let ctx = super::super::test_support::test_ctx();
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        let metrics = DispatchPathMetrics::new();
        let claim = drain::BashCallClaim::default();
        assert!(claim.claim_for_deadline_handoff());
        let (completion_tx, mut completion_rx) = mpsc::channel(1);
        send_ready_bash_result(
            &completion_tx,
            &metrics,
            route,
            116,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            root.clone(),
            "late-panic".into(),
            ToolCallResult {
                text: "forced panic".into(),
                response: Response::error("late-panic", "actor_fatal", "forced panic"),
            },
            &ctx,
            &crate::subc_format::FormatContext::default(),
            None,
            &claim,
            "late-task",
            "late-session",
        )
        .await;
        let done = tokio::time::timeout(Duration::from_secs(1), completion_rx.recv())
            .await
            .expect("late fatal must still reach the completion queue")
            .expect("late fatal observation");
        assert!(done.fatal);
        assert!(done.result.is_none(), "handoff already answered the caller");
        assert!(!done.settles_hold, "handoff already settled its accounting");

        let mut meta = RootMeta::new(Instant::now());
        meta.active_bash_waits = 1;
        let mut roots = HashMap::from([(root.clone(), meta)]);
        let mut cancels = HashMap::from([(
            route,
            RouteBashCancel {
                token: PersistentCancelSignal::new(),
                active_waits: 1,
            },
        )]);
        metrics.held_bash_calls.insert(route, 117);
        let (tx, mut rx) = mpsc::channel::<WriterFrame>(2);
        let shutdown = Arc::new(Notify::new());
        handle_bash_deferred_completion(
            &tx,
            done,
            &HashMap::new(),
            &mut roots,
            &mut cancels,
            &shutdown,
            &metrics,
        )
        .await
        .unwrap();
        assert_eq!(
            roots[&root].active_bash_waits, 1,
            "retain the sibling's hold"
        );
        assert_eq!(
            cancels[&route].active_waits, 1,
            "retain the sibling's cancellation entry"
        );
        assert!(metrics.held_bash_calls.request_cancel(route, 117));
        for channel in [1, 0] {
            let frame = rx.recv().await.expect("fatal goodbye").frame;
            assert_eq!(frame.header.ty, FrameType::Goodbye);
            assert_eq!(frame.header.channel, channel);
        }
        assert!(rx.try_recv().is_err(), "no second tool response");
        tokio::time::timeout(Duration::from_secs(1), shutdown.notified())
            .await
            .expect("fatal shutdown");
    }

    #[test]
    fn foreground_background_and_pty_share_submit_deferred_spawn_path() {
        let source = include_str!("bash.rs");
        let submit = source
            .split("pub(super) fn submit_deferred_bash")
            .nth(1)
            .and_then(|source| source.split("async fn run_deferred_bash_wait").next())
            .expect("submit_deferred_bash source");
        assert_eq!(
            submit.matches("dispatch(raw_req, ctx)").count(),
            1,
            "all bash modes must use the same authenticated dispatch"
        );
        let dispatch = submit.find("dispatch(raw_req, ctx)").unwrap();
        let mode_branch = submit
            .find("if launch_answers_now && (is_pty || settings.background)")
            .expect("background/PTY branch");
        let foreground_wait = submit
            .find("select_foreground_wait_window_ms")
            .expect("foreground branch");
        assert!(dispatch < mode_branch && mode_branch < foreground_wait);
        assert!(submit.contains("with_authenticated_principal"));
    }

    pub(super) fn running_bash_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let raw_params = req
            .params
            .get("params")
            .cloned()
            .unwrap_or_else(|| req.params.clone());
        let command = raw_params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("sleep 1");
        crate::bash_background::spawn(
            &req.id,
            req.session(),
            command,
            crate::bash_background::BashShell::Bash,
            std::path::PathBuf::from("/bin/bash"),
            None,
            None,
            crate::bash_background::HardKill::from_timeout_ms(
                raw_params.get("timeout").and_then(Value::as_u64),
            ),
            ctx,
            false,
            false,
            false,
            false,
            24,
            80,
            Vec::new(),
            false,
            None,
            None,
        )
    }

    // The deferred result is only useful after spawn; publish its task id so
    // the test can wait for the corresponding registry transition.
    static PENDING_MESSAGE_TASK_ID_TX: std::sync::LazyLock<
        std::sync::Mutex<Option<oneshot::Sender<String>>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

    fn running_bash_stub_with_task_id(req: RawRequest, ctx: &AppContext) -> Response {
        let response = running_bash_stub(req, ctx);
        if let Some(sender) = PENDING_MESSAGE_TASK_ID_TX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let task_id = response
                .data
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let _ = sender.send(task_id);
        }
        response
    }

    fn observe_worker_jail(req: RawRequest, _ctx: &AppContext) -> Response {
        Response::error_with_data(
            &req.id,
            "worker_jail_observation",
            "test dispatch observes the shell policy without spawning",
            json!({"worker_jail": crate::sandbox_spawn::worker_jail_active()}),
        )
    }

    fn observe_ledger_identity(req: RawRequest, _ctx: &AppContext) -> Response {
        Response::error_with_data(
            &req.id,
            "ledger_identity_observation",
            "test dispatch observes the ledger identity a task would record",
            json!({
                "task_requester": crate::bash_background::call_key_for_new_task("task").requester,
                "ledger_carrier": crate::bash_background::registry::ledger_spawn_for_test()
                    .map(|key| key.carrier),
            }),
        )
    }

    /// A codemode program's shell call spawns with its head agent's
    /// principal, but its task and its call-ledger row both name the run
    /// route's carrier, so the run route's late_results finds the result.
    #[tokio::test]
    async fn run_shell_task_and_ledger_row_name_the_run_route_carrier() {
        let (_dir, root) = super::super::test_support::test_root("run-shell-carrier");
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), super::super::test_support::test_ctx());
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (tx, mut rx) = mpsc::channel(8);
        let (touch, _touch_rx) = mpsc::channel(8);
        submit_deferred_bash(
            false,
            &executor,
            &tx,
            &touch,
            &metrics,
            observe_ledger_identity,
            root.clone(),
            root.as_path().into(),
            "head-session".into(),
            "run-shell-carrier".into(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            json!({"command":"echo test","timeout":5000}),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: true,
                ledger_carrier: Some("reserved:basal".into()),
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind {
                trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
                route_channel: 7,
                route_epoch: 1,
                project_root: root.as_path().into(),
                harness: "opencode".into(),
                session_id: "head-session".into(),
                principal_id: Some("direct".into()),
            },
            None,
            Some("program-shell".into()),
            None,
            None,
            None,
            false,
            false,
            false,
            true,
            super::remote_policy::RemoteSource::None,
            Instant::now(),
        );
        let done = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert_eq!(response.data["code"], "ledger_identity_observation");
        assert_eq!(response.data["task_requester"], "reserved:basal");
        assert_eq!(response.data["ledger_carrier"], "reserved:basal");
    }

    #[tokio::test]
    async fn consent_routing_does_not_choose_worker_shell_confinement() {
        for worker_jail in [false, true] {
            let (_dir, root) = super::super::test_support::test_root("consent-shell-policy");
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), super::super::test_support::test_ctx());
            let metrics = Arc::new(DispatchPathMetrics::new());
            let (tx, mut rx) = mpsc::channel(8);
            let (touch, _touch_rx) = mpsc::channel(8);
            {
                let _route = crate::access_policy::ask_route_guard(Some(
                    crate::access_policy::AskRoute::Consent {
                        once: Vec::new(),
                        launch: None,
                    },
                ));
                submit_deferred_bash(
                    false,
                    &executor,
                    &tx,
                    &touch,
                    &metrics,
                    observe_worker_jail,
                    root.clone(),
                    root.as_path().into(),
                    "consent-shell".into(),
                    "jail-observation".into(),
                    RouteChannel {
                        channel: 1,
                        epoch: 1,
                    },
                    1,
                    Flags::new(false, Priority::Interactive, false),
                    PROTOCOL_VERSION,
                    json!({"command":"echo test","timeout":5000}),
                    crate::subc_format::FormatContext::default(),
                    BashWaitCancel {
                        speech_seed: None,
                        run_scope: None,
                        scope: None,
                        late_results: false,
                        ledger_carrier: None,
                        connection: PersistentCancelSignal::new(),
                        route: PersistentCancelSignal::new(),
                        drain: drain::ModuleDrainWindow::default(),
                    },
                    BindTrust::FirstParty,
                    crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    worker_jail,
                    true,
                    super::remote_policy::RemoteSource::None,
                    Instant::now(),
                );
            }
            let done = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let response = done.response_for_test();
            assert_eq!(response.data["code"], "worker_jail_observation");
            assert_eq!(
                response.data["worker_jail"],
                json!(worker_jail),
                "heads ask without confinement; workers still require it"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scope_confirmation_kills_a_task_registered_after_its_snapshot() {
        let (_dir, root) = super::super::test_support::test_root("late-scope-registration");
        let ctx = super::super::test_support::test_ctx();
        let registry = ctx.bash_background().clone();
        let scope = subc_protocol::scope::ScopeStamp::new(
            Principal::Reserved {
                module_id: "owner".into(),
            },
            "scope",
            1,
            subc_protocol::scope::ScopeKind::Head,
        )
        .with_owner_authorized(true);
        registry.kill_scope(&scope);
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(8);
        let (touch_tx, _touch_rx) = mpsc::channel(8);
        submit_deferred_bash(
            false,
            &executor,
            &completion_tx,
            &touch_tx,
            &metrics,
            running_bash_stub,
            root.clone(),
            root.as_path().into(),
            "late-session".into(),
            "late-call".into(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            json!({"command":"sleep 2","timeout":10000,"foreground_orchestrate":true,"block_to_completion":true}),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: Some(scope),
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            true,
            super::remote_policy::RemoteSource::None,
            Instant::now(),
        );
        let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let response = completion.response_for_test();
        assert_eq!(response.data["status"], "killed", "{response:?}");
        let task = response.data["task_id"].as_str().unwrap();
        assert_eq!(
            registry
                .status(task, "late-session", None, None, 0)
                .unwrap()
                .info
                .status_reason
                .as_deref(),
            Some(crate::bash_background::registry::SCOPE_REVOKED_REASON)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v1_server_completion_never_returns_a_promoted_or_background_launch() {
        let detached = build_detached_frame(
            PROTOCOL_VERSION,
            1,
            1,
            1,
            Flags::new(false, Priority::Interactive, false),
            "task",
            false,
        )
        .unwrap();
        assert_eq!(detached.header.ty, FrameType::Error);
        let detached: Value = serde_json::from_slice(&detached.body).unwrap();
        assert_eq!(detached["code"], "detached");
        assert_eq!(
            detached["detail"],
            json!({"task_id":"task","late_results":false})
        );
        for (background, pty) in [(false, false), (true, false), (false, true)] {
            let (_dir, root) = super::super::test_support::test_root("v1-shell-completion");
            let ctx = super::super::test_support::test_ctx();
            ctx.update_config(|config| config.foreground_wait_window_ms = 10);
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), ctx);
            let metrics = Arc::new(DispatchPathMetrics::new());
            let (completion_tx, mut completion_rx) = mpsc::channel(8);
            let (touch_tx, _touch_rx) = mpsc::channel(8);
            let started = Instant::now();
            submit_deferred_bash(
                false,
                &executor,
                &completion_tx,
                &touch_tx,
                &metrics,
                running_bash_stub,
                root.clone(),
                root.as_path().into(),
                "v1-session".into(),
                "v1-shell".into(),
                RouteChannel {
                    channel: 1,
                    epoch: 1,
                },
                1,
                Flags::new(false, Priority::Interactive, false),
                PROTOCOL_VERSION,
                json!({"command":"sleep 0.2", "timeout":5000,"background":background,"pty":pty,"foreground_orchestrate":true,"block_to_completion":true}),
                crate::subc_format::FormatContext::default(),
                BashWaitCancel {
                    speech_seed: None,
                    run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(),
                },
                BindTrust::FirstParty,
                crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                None,
                None,
                None,
                None,
                None,
                false,
                false,
                false,
                true,
                super::remote_policy::RemoteSource::None,
                Instant::now(),
            );
            let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                started.elapsed() >= Duration::from_millis(100),
                "returned before command completion"
            );
            let response = completion.response_for_test();
            assert_ne!(
                response.data.get("status").and_then(Value::as_str),
                Some("running")
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(100), completion_rx.recv())
                    .await
                    .is_err()
            );
        }
    }

    struct DeferredWaitReleaseGuard(Vec<std::path::PathBuf>);

    fn deadline_test_call(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        dispatch: DispatchFn,
        arguments: Value,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        deadline_test_call_received(executor, root, dispatch, arguments, Instant::now())
    }

    fn pending_message_command() -> &'static str {
        // Windows can select cmd.exe, PowerShell, or Git Bash at runtime.
        if cfg!(windows) {
            "ping -n 2 127.0.0.1"
        } else {
            "sleep 1"
        }
    }

    #[tokio::test]
    async fn pending_message_detach_preserves_subc_wait_mode_eligibility() {
        const HANG_GUARD: Duration = Duration::from_secs(60);
        for wait in [true, false] {
            let (_dir, root) = super::super::test_support::test_root("pending-bash-message");
            let ctx = super::super::test_support::test_ctx();
            let registry = ctx.bash_background().clone();
            registry.request_wait_mode_detach("deadline-session");
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), ctx);
            let (task_id_tx, task_id_rx) = oneshot::channel();
            *PENDING_MESSAGE_TASK_ID_TX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task_id_tx);
            let mut arguments = json!({"command": pending_message_command(), "timeout": 10_000});
            if wait {
                arguments["wait"] = json!(true);
            }
            let mut replies =
                deadline_test_call(&executor, &root, running_bash_stub_with_task_id, arguments);
            let task_id = tokio::time::timeout(HANG_GUARD, task_id_rx)
                .await
                .expect("bash stub did not publish the spawned task id")
                .expect("bash stub dropped the task id sender");
            assert!(!task_id.is_empty(), "bash stub did not start a task");
            if wait {
                tokio::time::timeout(HANG_GUARD, async {
                    while !registry.wait_mode_detach_pending("deadline-session")
                        && replies.is_empty()
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("pending detach was neither attached to the wait nor answered");
            } else {
                tokio::time::timeout(HANG_GUARD, async {
                    while !registry.is_task_terminal(&task_id) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("bash task did not reach a terminal state");
            }
            let completion = tokio::time::timeout(HANG_GUARD, replies.recv())
                .await
                .expect("deferred bash response did not follow the task state transition")
                .unwrap();
            let response = completion.result.unwrap().response;
            if let Some(task_id) = response.data["task_id"].as_str() {
                let _ = registry.kill(task_id, "deadline-session");
            }
            assert!(response.success, "{response:?}");
            assert_eq!(
                response
                    .data
                    .to_string()
                    .contains("Detached because a user message arrived"),
                wait,
                "{response:?}"
            );
            if !wait {
                assert_eq!(response.data["status"], "completed", "{response:?}");
                assert_eq!(response.data["exit_code"], 0, "{response:?}");
            }
        }
    }

    fn deadline_test_call_received(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        dispatch: DispatchFn,
        arguments: Value,
        received_at: Instant,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        deadline_test_call_owned(executor, root, dispatch, arguments, received_at, false)
    }

    fn deadline_test_call_owned(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        dispatch: DispatchFn,
        arguments: Value,
        received_at: impl Into<crate::commands::blocking_wait::ReplyBudget>,
        server_completion: bool,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        let metrics = Arc::new(DispatchPathMetrics::new());
        deadline_test_call_controlled(
            executor,
            root,
            dispatch,
            arguments,
            received_at,
            server_completion,
            false,
            metrics,
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn deadline_test_call_controlled(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        dispatch: DispatchFn,
        arguments: Value,
        received_at: impl Into<crate::commands::blocking_wait::ReplyBudget>,
        server_completion: bool,
        worker_session: bool,
        metrics: Arc<DispatchPathMetrics>,
        cancel: BashWaitCancel,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        let (tx, rx) = mpsc::channel(8);
        let (touch_tx, _) = mpsc::channel(8);
        let repeat = crate::run_tool_call::RepeatObservation::for_agent_call(
            "deadline-session",
            "bash",
            &arguments,
            false,
            false,
            false,
        );
        submit_deferred_bash(
            false,
            executor,
            &tx,
            &touch_tx,
            &metrics,
            dispatch,
            root.clone(),
            root.as_path().into(),
            "deadline-session".into(),
            "deadline-call".into(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            arguments,
            crate::subc_format::FormatContext::default(),
            cancel,
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            repeat,
            worker_session,
            false,
            false,
            server_completion,
            super::remote_policy::RemoteSource::None,
            received_at,
        );
        rx
    }

    #[cfg(unix)]
    #[test]
    fn blocking_bash_hands_back_before_client_deadline() {
        fn held_running_stub(req: RawRequest, ctx: &AppContext) -> Response {
            let response = gated_running_stub(req, ctx);
            let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
            BashTimingForTest::wait(&timing.result, "spawn result");
            response
        }

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                for mode in ["wait", "block_to_completion"] {
                    let (_dir, root) =
                        super::super::test_support::test_root("bash-client-deadline");
                    let ctx = super::super::test_support::test_ctx();
                    let executor = Arc::new(Executor::new());
                    executor.register_actor(root.clone(), ctx.clone());
                    let started = tokio::time::Instant::now();
                    let mut timing = BashTimingForTest::new(started.into_std());
                    Arc::get_mut(&mut timing).unwrap().reply_uses_clock = true;
                    BashTimingForTest::release(&timing.admission);
                    let budget = crate::commands::blocking_wait::ReplyBudget::new(
                        started.into_std(),
                        Some((started + Duration::from_secs(3)).into_std()),
                    );
                    let mut args = json!({"command":"sleep 10","timeout":10000});
                    args[mode] = json!(true);
                    let mut rx = with_bash_timing_for_test(Some(timing.clone()), || {
                        deadline_test_call_owned(
                            &executor,
                            &root,
                            held_running_stub,
                            args,
                            budget,
                            false,
                        )
                    });
                    tokio::select! {
                        biased;
                        done = rx.recv() => panic!("{mode}: answered before registration: {:?}", done.map(|done| copy_response(done.response_for_test()))),
                        _ = timing.committed.notified() => {},
                    }

                    // Keep ordinary task observation off the blocking pool so only the
                    // Tokio reply-deadline timer can hand back the still-running task.
                    // Prevent polling the child before the blocker below holds
                    // the only pool thread: held_running_stub delays returning
                    // the task id until timing.result is released.
                    let (occupied_tx, occupied_rx) = std::sync::mpsc::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                    let blocker = tokio::task::spawn_blocking(move || {
                        occupied_tx.send(()).unwrap();
                        let _ = release_rx.recv();
                    });
                    occupied_rx.recv().unwrap();
                    BashTimingForTest::release(&timing.result);
                    tokio::select! {
                        biased;
                        done = rx.recv() => panic!("{mode}: answered before reply watchdog: {:?}", done.map(|done| copy_response(done.response_for_test()))),
                        _ = timing.reply.armed.notified() => {},
                    }
                    timing.reply.fire.notify_one();
                    tokio::time::advance(Duration::from_millis(1499)).await;
                    assert!(matches!(
                        rx.try_recv(),
                        Err(mpsc::error::TryRecvError::Empty)
                    ));

                    let cutoff = started + Duration::from_millis(2500);
                    // A running blocking job inhibits Tokio's automatic clock
                    // advancement, so drive the clock while it holds the pool.
                    let done = loop {
                        if let Ok(done) = rx.try_recv() {
                            break done;
                        }
                        assert!(
                            tokio::time::Instant::now() < cutoff,
                            "{mode}: blocking bash must hand back before transport expiry"
                        );
                        tokio::time::advance(Duration::from_millis(1)).await;
                        tokio::task::yield_now().await;
                    };
                    let replied_at = tokio::time::Instant::now();
                    drop(release_tx);
                    blocker.await.unwrap();
                    let response = done.response_for_test();
                    let task = response.data["task_id"].as_str().expect("running task id");

                    // Cleanup can outlive the client's deadline without making a timely
                    // reply late. Measure receipt, not synchronous process termination.
                    tokio::time::advance(Duration::from_secs(3)).await;
                    let _ = ctx.bash_background().kill(task, "deadline-session");
                    assert!(response.success, "{response:?}");
                    assert_eq!(response.data["status"], "running", "{response:?}");
                    assert!(
                        replied_at < cutoff,
                        "{mode}: reply arrived after safety cutoff: {:?}",
                        replied_at - started
                    );
                }
            });
    }

    #[tokio::test]
    async fn bash_deadline_counts_reader_queue_time_from_receipt() {
        let (_dir, root) = super::super::test_support::test_root("bash-receipt-clock");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| config.foreground_wait_window_ms = 2000);
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let mut rx = deadline_test_call_received(
            &executor,
            &root,
            slow_start_stub,
            json!({"command":"sleep 3", "timeout":60000}),
            Instant::now() - Duration::from_secs(3),
        );
        let done = tokio::time::timeout(Duration::from_millis(150), rx.recv())
            .await
            .expect("time spent in the reader queue is part of startup's reply budget")
            .unwrap();
        assert_eq!(done.response_for_test().data["code"], "bash_start_deadline");
    }

    /// The fake runner's connection file for [`remote_runon_stub`]; a dispatch
    /// function is a plain `fn`, so it cannot capture one.
    #[cfg(unix)]
    static REMOTE_RUNON_CONNECTION: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

    /// Runs the bash handler with a remote policy pointing at the fake runner.
    #[cfg(unix)]
    fn remote_runon_stub(mut req: RawRequest, ctx: &AppContext) -> Response {
        req.params
            .as_object_mut()
            .unwrap()
            .entry("workdir")
            .or_insert_with(|| json!(ctx.config().project_root));
        let launch = crate::bash_background::RemoteLaunch {
            explicit_runon: false,
            requested_platform: None,
            requested_vcpus: None,
            requested_network: false,
            configured_env_names: Vec::new(),
            return_paths: Vec::new(),
            connection_file: REMOTE_RUNON_CONNECTION
                .lock()
                .unwrap()
                .get(ctx.config().project_root.as_ref().unwrap())
                .cloned(),
            harness: "broca".into(),
            session: "session".into(),
            params: crate::exec_remote::FrozenParams {
                remote_exec: Some(crate::exec_remote::policy::RemoteExecPolicy {
                    enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        };
        crate::bash_background::with_remote_policy(Some(launch), || {
            crate::commands::bash::handle(&req, ctx)
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subc_blocking_runon_queued_past_its_timeout_answers_with_job_and_phase() {
        use crate::exec_remote::wire_tests::{daemon, id, Script};
        // The runner accepts the job at queue position 7 and keeps it queued
        // well past the call's `timeout`. Allow time for input capture on busy
        // test hosts so this fixture tests queued handback, not capture expiry.
        let daemon = daemon(
            Script::Staged {
                position: 7,
                queue: Duration::from_secs(20),
                run: Duration::ZERO,
            },
            "exec-remote/v1",
        )
        .await;
        let (dir, root) = super::super::test_support::test_root("remote-handback");
        REMOTE_RUNON_CONNECTION
            .lock()
            .unwrap()
            .insert(dir.path().into(), daemon.connection.clone());
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .unwrap()
            .success());
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.bash.runon_enabled = true;
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let timeout_ms = 10_000;
        let received = Instant::now();
        let mut rx = deadline_test_call(
            &executor,
            &root,
            remote_runon_stub,
            json!({"command":"uname -s", "runon":"linux", "wait":true, "timeout":timeout_ms, "foreground_orchestrate":true}),
        );
        // The OpenCode and Pi plugins stop waiting for this call 10 s after
        // its `timeout`.
        let transport_deadline = Duration::from_millis(timeout_ms + 10_000);
        let done = tokio::time::timeout(transport_deadline, rx.recv())
            .await
            .expect("a blocking runon call must answer before the transport deadline")
            .unwrap();
        let waited = received.elapsed();
        assert!(waited >= Duration::from_millis(timeout_ms), "{waited:?}");
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "running", "{response:?}");
        assert_eq!(response.data["remote_phase"], "queued", "{response:?}");
        assert_eq!(response.data["queue_position"], 7, "{response:?}");
        assert_eq!(response.data["remote_job_id"], id().to_string());
        let cancels = || {
            daemon
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|(header, body)| {
                    body["method"] == "exec.cancel" || header.ty == subc_protocol::FrameType::Cancel
                })
                .count()
        };
        assert_eq!(cancels(), 0, "the queued job must not be cancelled");
        // The queued runon job still leaves the queue and completes in the
        // background after the call returned.
        let task_id = response.data["task_id"].as_str().unwrap().to_string();
        let finished = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let snapshot = ctx
                    .bash_background()
                    .observed_status(&task_id, "deadline-session", 0)
                    .unwrap();
                if snapshot.info.status.is_terminal() {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            finished.info.status,
            crate::bash_background::BgTaskStatus::Completed
        );
        assert_eq!(cancels(), 0);
    }

    #[cfg(unix)]
    async fn capture_fixture(
        script: crate::exec_remote::wire_tests::Script,
    ) -> (
        crate::exec_remote::wire_tests::Daemon,
        tempfile::TempDir,
        ProjectRootId,
        Arc<Executor>,
        Arc<AppContext>,
    ) {
        capture_fixture_with_claims(script, &["exec-remote/v1"]).await
    }

    /// [`capture_fixture`] with a runner that advertises `claims`.
    #[cfg(unix)]
    async fn capture_fixture_with_claims(
        script: crate::exec_remote::wire_tests::Script,
        claims: &[&str],
    ) -> (
        crate::exec_remote::wire_tests::Daemon,
        tempfile::TempDir,
        ProjectRootId,
        Arc<Executor>,
        Arc<AppContext>,
    ) {
        let daemon =
            crate::exec_remote::wire_tests::daemon_with_capabilities(script, claims, 1).await;
        let (dir, root) = super::super::test_support::test_root("runon-capture");
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .unwrap()
            .success());
        REMOTE_RUNON_CONNECTION
            .lock()
            .unwrap()
            .insert(dir.path().into(), daemon.connection.clone());
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(dir.path().into());
            config.bash.runon_enabled = true;
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
            config.foreground_wait_window_ms = 15_000;
            config.experimental_bash_background = true;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        (daemon, dir, root, executor, ctx)
    }

    #[cfg(unix)]
    fn capture_call(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        arguments: Value,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        // These tests control the runner's first record, not how long AFT takes
        // to register the command. Freeze that startup deadline so a busy host
        // cannot refuse the fixture.
        with_bash_timing_for_test(Some(BashTimingForTest::new(Instant::now())), || {
            deadline_test_call(executor, root, remote_runon_stub, arguments)
        })
    }

    /// The remote bash handler, held after it has registered the task and
    /// before it returns, until the test releases `BashTimingForTest::result`.
    /// The launch reply is decided only after this dispatch returns, so a test
    /// can let the runner accept or finish the job first and fix that ordering.
    #[cfg(unix)]
    fn held_remote_runon_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
        let response = remote_runon_stub(req, ctx);
        *timing.task_id.lock().unwrap() = response.data["task_id"].as_str().map(str::to_owned);
        timing.committed.notify_one();
        let (released, _) = timing
            .result
            .1
            .wait_timeout_while(
                timing.result.0.lock().unwrap(),
                Duration::from_secs(60),
                |released| !*released,
            )
            .unwrap();
        assert!(*released, "the held remote dispatch was never released");
        response
    }

    /// Starts a call through [`held_remote_runon_stub`] with the given timing
    /// and wait signals. Returns once the task is registered, with its id; the
    /// dispatch stays held until the test releases `timing.result`.
    #[cfg(unix)]
    async fn held_capture_call(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        arguments: Value,
        timing: Arc<BashTimingForTest>,
        cancel: BashWaitCancel,
    ) -> (mpsc::Receiver<BashDeferredCompletion>, String) {
        held_capture_call_with_metrics(
            executor,
            root,
            arguments,
            timing,
            cancel,
            Arc::new(DispatchPathMetrics::new()),
            Instant::now(),
        )
        .await
    }

    /// [`held_capture_call`] with the caller's dispatch metrics, which hold the
    /// call's entry in the held-bash-call table the module loop answers from.
    #[cfg(unix)]
    async fn held_capture_call_with_metrics(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        arguments: Value,
        timing: Arc<BashTimingForTest>,
        cancel: BashWaitCancel,
        metrics: Arc<DispatchPathMetrics>,
        received_at: Instant,
    ) -> (mpsc::Receiver<BashDeferredCompletion>, String) {
        let rx = with_bash_timing_for_test(Some(timing.clone()), || {
            deadline_test_call_controlled(
                executor,
                root,
                held_remote_runon_stub,
                arguments,
                received_at,
                false,
                false,
                metrics,
                cancel,
            )
        });
        tokio::time::timeout(Duration::from_secs(60), timing.committed.notified())
            .await
            .expect("the remote task must be registered");
        let task_id = timing.task_id.lock().unwrap().clone();
        (
            rx,
            task_id.expect("the remote handler must return a task id"),
        )
    }

    /// Polls the task registry until `ready` holds; `what` names the state
    /// in the failure message. The timeout is only a hang watchdog.
    #[cfg(unix)]
    async fn wait_for_task_state(
        ctx: &AppContext,
        what: &str,
        ready: impl Fn(&crate::bash_background::BgTaskRegistry) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(60), async {
            while !ready(ctx.bash_background()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the remote task never reached: {what}"));
    }

    #[cfg(unix)]
    fn capture_cancel() -> BashWaitCancel {
        BashWaitCancel {
            speech_seed: None,
            run_scope: None,
            scope: None,
            late_results: false,
            ledger_carrier: None,
            connection: PersistentCancelSignal::new(),
            route: PersistentCancelSignal::new(),
            drain: drain::ModuleDrainWindow::default(),
        }
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum ReplyDecided {
        AfterAcceptance,
        BeforeAcceptance,
    }

    #[cfg(unix)]
    fn assert_launch_reply(
        done: &BashDeferredCompletion,
        task_id: &str,
        case: &str,
        decided: ReplyDecided,
    ) {
        let response = done.response_for_test();
        let text = done.text_for_test().unwrap();
        assert!(response.success, "{case}: {response:?}");
        assert_eq!(response.data["task_id"], task_id, "{case}: {response:?}");
        match decided {
            ReplyDecided::AfterAcceptance => {
                assert_eq!(response.data["status"], "running", "{case}: {response:?}");
                assert_ne!(
                    response.data["remote_phase"], "submitting",
                    "{case}: accepted launch must not appear pending: {response:?}"
                );
                assert!(
                    response.data["remote_job_id"].as_str().is_some(),
                    "{case}: accepted launch must carry the runner job ID: {response:?}"
                );
                assert!(
                    !text.contains(REMOTE_ADMISSION_PENDING_NOTE),
                    "{case}: accepted launch must not carry the no-rerun note: {text}"
                );
                assert!(
                    !text.contains(REMOTE_INPUT_CAPTURE_WARNING),
                    "{case}: {text}"
                );
                assert!(
                    !response.data.to_string().contains("hasn't confirmed"),
                    "{case}: {response:?}"
                );
            }
            ReplyDecided::BeforeAcceptance => {
                assert_eq!(response.data["status"], "starting", "{case}: {response:?}");
                assert_eq!(
                    response.data["remote_phase"], "submitting",
                    "{case}: {response:?}"
                );
                assert!(
                    response.data["remote_job_id"].is_null(),
                    "{case}: {response:?}"
                );
                assert!(
                    text.contains(REMOTE_ADMISSION_PENDING_NOTE),
                    "{case}: {text}"
                );
                assert!(
                    text.contains(REMOTE_INPUT_CAPTURE_WARNING),
                    "{case}: {text}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_draining_retry_waits_for_accepted() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::DrainingGatedAcceptance).await;
        let timing = BashTimingForTest::new(Instant::now());
        let (mut rx, registered_task) = held_capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":600_000}),
            timing.clone(),
            capture_cancel(),
        )
        .await;
        daemon.wait_for_requests("exec.run", 2).await;
        assert_eq!(
            ctx.bash_background()
                .runon_inputs_captured(&registered_task, "deadline-session"),
            Some(false)
        );
        BashTimingForTest::release(&timing.result);
        let launch = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("the gated retry must not hold the launch reply")
            .unwrap();
        let response = launch.response_for_test();
        let task_id = response.data["task_id"].as_str().unwrap().to_owned();
        assert_eq!(response.data["status"], "starting", "{response:?}");
        assert!(launch
            .text_for_test()
            .unwrap()
            .contains(REMOTE_INPUT_CAPTURE_WARNING));
        assert_eq!(task_id, registered_task);
        assert!(rx.try_recv().is_err(), "the call has one launch reply");
        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "retry accepted", |registry| {
            registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
        })
        .await;
        assert!(
            rx.try_recv().is_err(),
            "acceptance does not send a second launch reply"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_runon_draining_retry_can_be_killed_after_launch_reply() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) = capture_fixture(Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10000),
        })
        .await;
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000}),
        );
        daemon.wait_for_requests("exec.run", 1).await;
        let launch = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("the background task id must be returned before the draining retry")
            .unwrap();
        let task_id = launch.response_for_test().data["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(launch.response_for_test().data["status"], "starting");
        assert!(launch
            .text_for_test()
            .unwrap()
            .contains(REMOTE_INPUT_CAPTURE_WARNING));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let task = ctx.bash_background().task_for_test(&task_id).unwrap();
                if task
                    .state
                    .lock()
                    .unwrap()
                    .metadata
                    .remote
                    .as_ref()
                    .unwrap()
                    .terminal
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("draining refusal must arrive before cancellation");
        assert_eq!(
            ctx.bash_background()
                .runon_inputs_captured(&task_id, "deadline-session"),
            Some(false)
        );
        let snapshot = ctx
            .bash_background()
            .observed_status(&task_id, "deadline-session", 0)
            .unwrap();
        assert_eq!(
            snapshot.info.status,
            crate::bash_background::BgTaskStatus::Starting
        );
        assert!(snapshot
            .remote_phase_note
            .unwrap()
            .contains("capturing inputs"));
        let registry = ctx.bash_background().clone();
        let killed_task = task_id.clone();
        tokio::task::spawn_blocking(move || registry.kill(&killed_task, "deadline-session"))
            .await
            .unwrap()
            .unwrap();
        wait_for_task_state(&ctx, "killed", |registry| {
            registry.is_task_terminal(&task_id)
        })
        .await;
        wait_for_task_state(&ctx, "kill completion queued", |registry| {
            registry.has_completions_for_session(Some("deadline-session"))
        })
        .await;
        let completion = ctx
            .bash_background()
            .drain_completions_for_session(Some("deadline-session"))
            .into_iter()
            .find(|completion| completion.task_id == task_id)
            .expect("the killed task's completion must be retained");
        assert_eq!(
            completion.status,
            crate::bash_background::BgTaskStatus::Killed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_blocking_wait_keeps_its_own_limit() {
        use crate::exec_remote::wire_tests::Script;
        for mode in ["wait", "block_to_completion"] {
            let (daemon, _dir, root, executor, ctx) =
                capture_fixture(Script::GatedAcceptance).await;
            // Bound the caller's wait, not the remote command's lifetime: a busy
            // host must not end the registered task before the runner receives it.
            ctx.update_config(|config| config.bash.worker_wait_max_ms = 700);
            let mut timing = BashTimingForTest::new(Instant::now());
            Arc::get_mut(&mut timing).unwrap().capture_wait = Some(Duration::from_millis(150));
            let started = Instant::now();
            let mut args = json!({"command":"uname -s", "runon":"linux", "timeout":600_000});
            args[mode] = json!(true);
            let (mut rx, task_id) =
                held_capture_call(&executor, &root, args, timing.clone(), capture_cancel()).await;
            daemon.wait_for_requests("exec.run", 1).await;
            BashTimingForTest::release(&timing.result);
            let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                started.elapsed() >= Duration::from_millis(700),
                "{mode} ended at the capture deadline instead of its own limit"
            );
            assert!(done
                .text_for_test()
                .unwrap()
                .contains(REMOTE_INPUT_CAPTURE_WARNING));
            assert_eq!(done.response_for_test().data["task_id"], task_id);
            assert_eq!(done.response_for_test().data["remote_phase"], "submitting");
            assert!(!ctx.bash_background().is_task_terminal(&task_id));
            daemon.acceptance_gate.notify_one();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_status_is_starting_until_accepted() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) = capture_fixture(Script::GatedAcceptance).await;
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000}),
        );
        daemon.wait_for_requests("exec.run", 1).await;
        let launch = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("background runon must reply before Accepted")
            .unwrap();
        let response = launch.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap().to_owned();
        assert_eq!(response.data["status"], "starting", "{response:?}");
        assert_eq!(response.data["remote_phase"], "submitting", "{response:?}");
        assert!(launch
            .text_for_test()
            .unwrap()
            .contains(REMOTE_ADMISSION_PENDING_NOTE));
        let status = || {
            crate::commands::bash_status::handle(
                &RawRequest {
                    id: "capture-status".into(),
                    command: "bash_status".into(),
                    params: json!({"task_id":task_id}),
                    session_id: Some("deadline-session".into()),
                    lsp_hints: None,
                },
                &ctx,
            )
        };
        let before = status();
        assert_eq!(before.data["status"], "starting", "{before:?}");
        assert_eq!(before.data["phase"], "submitting");
        assert!(before.data["remote_phase_note"]
            .as_str()
            .unwrap()
            .contains("capturing inputs"));
        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "runner acceptance", |registry| {
            registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
        })
        .await;
        let after = status();
        assert_eq!(after.data["phase"], "queued");
        assert!(after.data["remote_phase_note"]
            .as_str()
            .unwrap()
            .contains("queued on the runner"));
        assert!(
            rx.try_recv().is_err(),
            "acceptance does not send a second reply"
        );
    }

    #[cfg(unix)]
    async fn capture_interrupt_call(drain: bool) {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) = capture_fixture(Script::GatedAcceptance).await;
        let metrics = Arc::new(DispatchPathMetrics::new());
        let cancel = BashWaitCancel {
            speech_seed: None,
            run_scope: None,
            scope: None,
            late_results: false,
            ledger_carrier: None,
            connection: PersistentCancelSignal::new(),
            route: PersistentCancelSignal::new(),
            drain: drain::ModuleDrainWindow::default(),
        };
        let mut rx =
            with_bash_timing_for_test(Some(BashTimingForTest::new(Instant::now())), || {
                deadline_test_call_controlled(
                    &executor,
                    &root,
                    remote_runon_stub,
                    json!({"command":"uname -s", "runon":"linux", "timeout":10000}),
                    Instant::now(),
                    false,
                    false,
                    metrics.clone(),
                    cancel.clone(),
                )
            });
        daemon.wait_for_requests("exec.run", 1).await;
        let route = RouteChannel {
            channel: 1,
            epoch: 1,
        };
        if !drain {
            metrics.held_bash_calls.request_cancel(route, 1);
        }
        let task_id = ctx
            .bash_background()
            .list(0)
            .into_iter()
            .next()
            .unwrap()
            .info
            .task_id;
        let routes = HashMap::from([(
            route,
            super::super::test_support::route_identity(&root, "deadline-session"),
        )]);
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);
        tokio::time::timeout(Duration::from_millis(750), async {
            loop {
                if answer_held_bash_calls_from_module_loop(&writer_tx, &routes, &metrics, drain)
                    .await
                    .unwrap()
                    == 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("capture wait must expose its task to the module loop promptly");
        let frame = writer_rx.recv().await.unwrap().frame;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert!(body.to_string().contains(&task_id), "{body}");
        if drain {
            cancel.drain.begin(Instant::now() + Duration::from_secs(10));
        } else {
            assert_eq!(body["code"], "cancelled");
            assert_eq!(body["detail"]["task_id"], task_id);
        }
        let done = tokio::time::timeout(Duration::from_millis(750), rx.recv())
            .await
            .expect("capture wait must observe the module-loop claim")
            .unwrap();
        assert!(done.result.is_none(), "module loop owns the only reply");
        assert!(writer_rx.try_recv().is_err(), "one terminal reply per call");
        daemon.acceptance_gate.notify_one();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_client_cancel_releases_task_handle() {
        capture_interrupt_call(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_module_drain_releases_task_handle() {
        capture_interrupt_call(true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_restart_module_loop_reports_pending_until_accepted() {
        use crate::exec_remote::wire_tests::Script;
        for background in [false] {
            let (daemon, _dir, root, executor, ctx) =
                capture_fixture(Script::GatedAcceptance).await;
            let metrics = Arc::new(DispatchPathMetrics::new());
            let cancel = BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            };
            let timing = BashTimingForTest::new(Instant::now());
            let mut rx = with_bash_timing_for_test(Some(timing.clone()), || {
                deadline_test_call_controlled(
                    &executor,
                    &root,
                    remote_runon_stub,
                    json!({"command":"uname -s", "runon":"linux", "background":background, "timeout":10000}),
                    Instant::now(),
                    false,
                    false,
                    metrics.clone(),
                    cancel.clone(),
                )
            });
            daemon.wait_for_requests("exec.run", 1).await;
            tokio::time::timeout(Duration::from_secs(5), timing.capture_registered.notified())
                .await
                .expect("capture wait must publish its detach target");
            let task_id = ctx.bash_background().list(0)[0].info.task_id.clone();
            let route = RouteChannel {
                channel: 1,
                epoch: 1,
            };
            let routes = HashMap::from([(
                route,
                super::super::test_support::route_identity(&root, "deadline-session"),
            )]);
            let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);
            // Simulate the module's restart deadline: its frame loop must answer
            // even though the capture waiter has not observed the drain yet.
            let expected = stateful_module_drain_reply_for_test(
                ctx.bash_background(),
                &task_id,
                "deadline-session",
                "deadline-call",
                false,
                false,
            );
            let task = ctx.bash_background().task_for_test(&task_id).unwrap();
            let state = task.state.lock().unwrap();
            let no_task_locks =
                crate::bash_background::registry::forbid_task_state_locks_for_test();
            assert_eq!(
                answer_held_bash_calls_from_module_loop(&writer_tx, &routes, &metrics, true)
                    .await
                    .unwrap(),
                1
            );
            drop(no_task_locks);
            drop(state);
            let frame = writer_rx.recv().await.unwrap().frame;
            let body: Value = serde_json::from_slice(&frame.body).unwrap();
            assert_eq!(
                body["structuredContent"]["output"], expected.data["output"],
                "uncaptured run-on drain reply must keep its original text"
            );
            assert_eq!(
                body["content"][0]["text"],
                bash_result_from_response(expected, &Default::default()).text
            );
            assert_eq!(body["structuredContent"]["task_id"], task_id, "{body}");
            assert_eq!(body["structuredContent"]["status"], "starting", "{body}");
            assert!(
                body.to_string().contains(REMOTE_INPUT_CAPTURE_WARNING),
                "{body}"
            );
            let done = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(done.result.is_none(), "the module loop owns the only reply");
            assert!(writer_rx.try_recv().is_err());

            let status = || {
                crate::commands::bash_status::handle(
                    &RawRequest {
                        id: "restart-capture-status".into(),
                        command: "bash_status".into(),
                        params: json!({"task_id":task_id}),
                        session_id: Some("deadline-session".into()),
                        lsp_hints: None,
                    },
                    &ctx,
                )
            };
            let before = status();
            assert_eq!(before.data["status"], "starting", "{before:?}");
            assert_eq!(before.data["phase"], "submitting");
            assert!(
                before.data["output_preview"]
                    .as_str()
                    .unwrap()
                    .contains(REMOTE_INPUT_CAPTURE_WARNING),
                "{before:?}"
            );

            daemon.acceptance_gate.notify_one();
            let claim = drain::BashCallClaim::default();
            let response = Response::success("capture-observer", json!({"task_id":task_id}));
            assert!(
                await_runon_launch_reply(
                    ctx.bash_background().clone(),
                    "deadline-session",
                    &response,
                    Some(Instant::now() + Duration::from_secs(5)),
                    &cancel,
                    &claim,
                    false,
                    None,
                    false,
                )
                .await
                .is_none(),
                "Accepted must release the capture wait"
            );
            let after = status();
            assert_eq!(after.data["status"], "running", "{after:?}");
            assert_eq!(after.data["phase"], "queued");
            assert!(!after.data["output_preview"]
                .as_str()
                .unwrap()
                .contains(REMOTE_INPUT_CAPTURE_WARNING));
        }
    }

    /// The module loop answers a held foreground call itself when a drain is
    /// due (see `answer_held_bash_calls_from_module_loop`). Once the runner's
    /// Accepted is recorded, that answer is the drain-detach reply for a live
    /// task: status `running`, without the input-capture warning. The answer
    /// before Accepted is covered by
    /// `runon_capture_restart_module_loop_reports_pending_until_accepted`.
    /// The remote handler is held until Accepted is recorded, so the call
    /// reaches the module loop's held-call table only afterwards.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_module_loop_drain_after_accepted_keeps_running_reply() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::GatedStages { position: 1 }).await;
        // Keep the foreground wait from promoting the call on its own before
        // the module loop answers it.
        ctx.update_config(|config| config.foreground_wait_window_ms = 600_000);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let timing = BashTimingForTest::new(Instant::now());
        let (mut rx, task_id) = held_capture_call_with_metrics(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "timeout":10000}),
            timing.clone(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            metrics.clone(),
            Instant::now(),
        )
        .await;
        wait_for_task_state(&ctx, "Accepted", |registry| {
            registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
        })
        .await;
        BashTimingForTest::release(&timing.result);
        let routes = HashMap::from([(
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            super::super::test_support::route_identity(&root, "deadline-session"),
        )]);
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);
        tokio::time::timeout(Duration::from_secs(60), async {
            while answer_held_bash_calls_from_module_loop(&writer_tx, &routes, &metrics, true)
                .await
                .unwrap()
                == 0
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the foreground call must reach the module loop's held-call table");
        let expected = stateful_module_drain_reply_for_test(
            ctx.bash_background(),
            &task_id,
            "deadline-session",
            "deadline-call",
            false,
            false,
        );
        let frame = writer_rx.recv().await.unwrap().frame;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(
            body["structuredContent"]["output"], expected.data["output"],
            "accepted run-on drain reply must keep its original text"
        );
        assert_eq!(
            body["content"][0]["text"],
            bash_result_from_response(expected, &Default::default()).text
        );
        assert_eq!(body["structuredContent"]["task_id"], task_id, "{body}");
        assert_eq!(body["structuredContent"]["status"], "running", "{body}");
        assert!(
            !body.to_string().contains(REMOTE_INPUT_CAPTURE_WARNING),
            "{body}"
        );
        assert!(!body.to_string().contains("hasn't confirmed"), "{body}");
        let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(done.result.is_none(), "the module loop owns the only reply");
        assert!(writer_rx.try_recv().is_err(), "one terminal reply per call");
        assert!(
            !ctx.bash_background().is_task_terminal(&task_id),
            "the reply described a live task"
        );
        daemon.output_gate.notify_one();
        wait_for_task_state(&ctx, "completed", |registry| {
            daemon.completion_gate.notify_one();
            registry.is_task_terminal(&task_id)
        })
        .await;
    }

    /// Cancellation and an AFT restart each end a foreground capture wait on
    /// their own. No timer competes with them here: the capture deadline is
    /// pushed out to an hour and the foreground window to ten minutes, and the
    /// runner holds Accepted until the test releases it.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_cancellation_and_drain_signals_interrupt_wait() {
        use crate::exec_remote::wire_tests::Script;
        for drain in [false, true] {
            let (daemon, _dir, root, executor, ctx) =
                capture_fixture(Script::GatedAcceptance).await;
            ctx.update_config(|config| config.foreground_wait_window_ms = 600_000);
            let metrics = Arc::new(DispatchPathMetrics::new());
            let cancel = BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            };
            let mut timing = BashTimingForTest::new(Instant::now());
            Arc::get_mut(&mut timing).unwrap().capture_wait = Some(Duration::from_secs(3600));
            let mut rx = with_bash_timing_for_test(Some(timing.clone()), || {
                deadline_test_call_controlled(
                    &executor,
                    &root,
                    remote_runon_stub,
                    json!({"command":"uname -s", "runon":"linux", "background":false, "timeout":10000}),
                    Instant::now(),
                    false,
                    false,
                    metrics,
                    cancel.clone(),
                )
            });
            daemon.wait_for_requests("exec.run", 1).await;
            tokio::time::timeout(
                Duration::from_secs(60),
                timing.capture_registered.notified(),
            )
            .await
            .expect("capture wait must publish its detach target");
            assert!(
                rx.try_recv().is_err(),
                "the foreground capture wait holds the reply"
            );
            let task_id = ctx
                .bash_background()
                .list(0)
                .into_iter()
                .next()
                .unwrap()
                .info
                .task_id;
            if drain {
                cancel.drain.begin(Instant::now() + Duration::from_secs(10));
            } else {
                cancel.connection.cancel();
            }
            let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
                .await
                .expect("capture wait must observe cancellation and drain signals")
                .unwrap();
            if drain {
                assert_eq!(done.response_for_test().data["task_id"], task_id);
                assert_eq!(done.response_for_test().data["status"], "starting");
                assert_eq!(done.response_for_test().data["phase"], "submitting");
                assert!(done
                    .text_for_test()
                    .unwrap()
                    .contains(REMOTE_INPUT_CAPTURE_WARNING));
            } else {
                assert!(done.result.is_none());
                assert!(
                    ctx.bash_background()
                        .take_cancelled_call_notices("deadline-session")
                        .iter()
                        .any(|notice| notice.contains(&task_id)),
                    "cancelled call must hand off the task id"
                );
            }
            daemon.acceptance_gate.notify_one();
        }
    }

    /// A background runon reply keeps local background launch wording and names
    /// the registered task before acceptance, then delivers completion on that id.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_runon_replies_at_registration_before_acceptance() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::GatedAcceptanceThenExit).await;
        let received_at = Instant::now();
        let timing = BashTimingForTest::new(received_at);
        let mut rx = with_bash_timing_for_test(Some(timing.clone()), || {
            deadline_test_call_received(
                &executor,
                &root,
                remote_runon_stub,
                json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":600_000}),
                received_at,
            )
        });
        tokio::time::timeout(Duration::from_secs(5), timing.capture_registered.notified())
            .await
            .expect("task registration must publish its detach target");
        let registered_at = Instant::now();
        let early = tokio::time::timeout(Duration::from_millis(900), rx.recv()).await;
        if early.is_err() {
            daemon.acceptance_gate.notify_one();
            let _ = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
            panic!("background runon must reply within one second of task registration, before Accepted");
        }
        let done = early.unwrap().unwrap();
        assert!(registered_at.elapsed() < Duration::from_secs(1));
        assert!(
            received_at.elapsed() < Duration::from_secs(5),
            "background runon must not wait for the 15-second foreground window"
        );
        let response = done.response_for_test();
        let text = done.text_for_test().unwrap();
        let task_id = response.data["task_id"].as_str().unwrap().to_owned();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "starting", "{response:?}");
        assert_eq!(response.data["remote_phase"], "submitting", "{response:?}");
        assert_eq!(
            response.data["output"].as_str().unwrap(),
            format!(
                "Background task started: {task_id}. A completion reminder will be delivered automatically; don't poll bash_status.\n{REMOTE_ADMISSION_PENDING_NOTE}\n{REMOTE_INPUT_CAPTURE_WARNING}"
            ),
        );
        assert!(!text.contains("Foreground bash didn't finish"), "{text}");
        assert!(text.contains(&task_id), "{text}");
        assert!(text.contains(REMOTE_ADMISSION_PENDING_NOTE), "{text}");
        assert!(text.contains(REMOTE_INPUT_CAPTURE_WARNING), "{text}");
        assert!(ctx
            .bash_background()
            .remote_progress(&task_id, "deadline-session")
            .unwrap()
            .job_id
            .is_none());

        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "completed", |registry| {
            registry.is_task_terminal(&task_id)
        })
        .await;
        wait_for_task_state(&ctx, "completion queued", |registry| {
            registry.has_completions_for_session(Some("deadline-session"))
        })
        .await;
        assert!(ctx
            .bash_background()
            .drain_completions_for_session(Some("deadline-session"))
            .iter()
            .any(|completion| completion.task_id == task_id));
    }

    /// A background runon with return_paths still replies at registration with
    /// its task id; the requested files are copied back when the job ends and
    /// its completion lists them.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_runon_with_return_paths_replies_at_once_and_copies_back_at_completion() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, dir, root, executor, ctx) = capture_fixture_with_claims(
            Script::GatedReturnVector("failed-missing"),
            &["exec-remote/v1", "exec-remote-return/v2"],
        )
        .await;
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"make dist", "runon":"linux", "background":true, "timeout":600_000,
                   "return_paths":["dist/hello.txt", "dist/absent.txt"]}),
        );
        daemon.wait_for_requests("exec.run", 1).await;
        let done = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("background launch must answer before the runner accepts")
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap().to_owned();
        assert_eq!(response.data["status"], "starting", "{response:?}");
        let hello = dir.path().join("dist/hello.txt");
        assert!(
            !hello.exists(),
            "nothing is copied back before the job ends"
        );
        let run = daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .find(|(_, body)| body["method"] == "exec.run")
            .map(|(_, body)| body["params"].clone())
            .unwrap();
        assert_eq!(
            run["return_paths"],
            json!(["dist/hello.txt", "dist/absent.txt"])
        );
        assert_eq!(run["return_chunked"], true);

        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "completed", |registry| {
            registry.is_task_terminal(&task_id)
        })
        .await;
        wait_for_task_state(&ctx, "completion queued", |registry| {
            registry.has_completions_for_session(Some("deadline-session"))
        })
        .await;
        let completion = ctx
            .bash_background()
            .drain_completions_for_session(Some("deadline-session"))
            .into_iter()
            .find(|completion| completion.task_id == task_id)
            .expect("the task's completion must reach the caller");
        assert_eq!(std::fs::read(&hello).unwrap(), b"hello");
        assert!(!dir.path().join("dist/absent.txt").exists());
        assert!(
            completion.output_preview.contains(
                "Files requested back from the server (return_paths):\n  \
                 dist/hello.txt: returned (5 bytes)\n  \
                 dist/absent.txt: not returned (no such file on the server)"
            ),
            "{}",
            completion.output_preview
        );
    }

    /// A runner refusal after the early launch reply still reaches the caller
    /// through the completion for the task id that reply named.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_runon_refusal_after_reply_is_a_task_completion() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::GatedAcceptanceThenRefused).await;
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":600_000}),
        );
        daemon.wait_for_requests("exec.run", 1).await;
        let done = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("background launch must answer before the runner accepts")
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap().to_owned();
        assert_eq!(response.data["status"], "starting", "{response:?}");
        assert!(done
            .text_for_test()
            .unwrap()
            .contains(REMOTE_ADMISSION_PENDING_NOTE));

        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "runner refusal recorded", |registry| {
            registry
                .observed_status(&task_id, "deadline-session", 0)
                .is_some_and(|snapshot| snapshot.remote_refusal.is_some())
        })
        .await;
        wait_for_task_state(&ctx, "refusal completion queued", |registry| {
            registry.has_completions_for_session(Some("deadline-session"))
        })
        .await;
        let completion = ctx
            .bash_background()
            .drain_completions_for_session(Some("deadline-session"))
            .into_iter()
            .find(|completion| completion.task_id == task_id)
            .expect("the refused task's completion must reach the caller");
        assert!(completion
            .status_reason
            .as_deref()
            .is_some_and(|reason| reason.to_ascii_lowercase().contains("unreachable")));
    }

    /// A kill recorded while the background task's worker is still opening
    /// its runner route (before `exec.run` is sent) ends the task there: the
    /// worker checks for the cancellation before dispatch and never submits
    /// the command.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_runon_kill_before_submission_never_dispatches() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) = capture_fixture(Script::GatedRoute).await;
        let mut replies = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "background":true}),
        );
        let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .expect("background launch must not wait for route opening")
            .unwrap();
        let task_id = reply.response_for_test().data["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let killed_id = task_id.clone();
        let kill_ctx = ctx.clone();
        let kill = tokio::task::spawn_blocking(move || {
            crate::commands::bash_kill::handle(
                &RawRequest {
                    id: "kill-before-submit".into(),
                    command: "bash_kill".into(),
                    lsp_hints: None,
                    session_id: Some("deadline-session".into()),
                    params: json!({"task_id":killed_id}),
                },
                &kill_ctx,
            )
        });
        wait_for_task_state(&ctx, "cancel intent before dispatch", |registry| {
            registry
                .task_for_test(&task_id)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .cancel_requested
        })
        .await;
        daemon.acceptance_gate.notify_one();
        let killed = tokio::time::timeout(Duration::from_secs(5), kill)
            .await
            .expect("cancelled startup must settle once its connection resolves")
            .unwrap();
        assert_eq!(killed.data["status"], "killed", "{killed:?}");
        assert!(
            !daemon
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|(_, body)| body["method"] == "exec.run"),
            "a cancellation recorded before submission must prevent exec.run"
        );
    }

    /// Start a background runon whose runner holds admission, and return its
    /// launch reply (the task id, before Accepted) with the caller's transport
    /// deadline.
    #[cfg(unix)]
    async fn background_admission_reply(
        daemon: &crate::exec_remote::wire_tests::Daemon,
        executor: &Arc<Executor>,
        root: &ProjectRootId,
    ) -> (BashDeferredCompletion, Instant) {
        let received = Instant::now();
        let transport_deadline = received + Duration::from_secs(5);
        let budget =
            crate::commands::blocking_wait::ReplyBudget::new(received, Some(transport_deadline));
        let mut replies = with_bash_timing_for_test(Some(BashTimingForTest::new(received)), || {
            deadline_test_call_owned(
                executor,
                root,
                remote_runon_stub,
                json!({"command":"uname -s", "runon":"linux,4c", "background":true}),
                budget,
                false,
            )
        });
        daemon.wait_for_requests("exec.run", 1).await;
        let reply = tokio::time::timeout(Duration::from_secs(1), replies.recv())
            .await
            .expect("background runon must return its task id without waiting for remote admission")
            .unwrap();
        assert!(
            reply.response_for_test().success,
            "{:?}",
            reply.response_for_test()
        );
        assert!(reply.response_for_test().data["task_id"].is_string());
        assert!(
            Instant::now() < transport_deadline,
            "task id must arrive inside the transport budget"
        );
        (reply, transport_deadline)
    }

    #[cfg(unix)]
    fn admission_status(ctx: &AppContext, task_id: &str) -> Response {
        crate::commands::bash_status::handle(
            &RawRequest {
                id: "admission-status".into(),
                command: "bash_status".into(),
                lsp_hints: None,
                session_id: Some("deadline-session".into()),
                params: json!({"task_id":task_id}),
            },
            ctx,
        )
    }

    /// A kill recorded after the runner received `exec.run` but before it
    /// accepted the job has no job id to cancel. The worker cancels the call
    /// itself: a call-key Cancel on the same channel, epoch and correlation as
    /// the `exec.run`, kept alive by a status round trip so the worker's
    /// runtime shutting down cannot drop it. Without terminal proof the task
    /// ends as fate unknown, never as having run.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_runon_kill_before_admission_cancels_submission() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::GatedAdmission { refuse: false }).await;
        let (reply, _) = background_admission_reply(&daemon, &executor, &root).await;
        let task_id = reply.response_for_test().data["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let killed_id = task_id.clone();
        let kill_ctx = ctx.clone();
        let killed = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                crate::commands::bash_kill::handle(
                    &RawRequest {
                        id: "kill-admission".into(),
                        command: "bash_kill".into(),
                        lsp_hints: None,
                        session_id: Some("deadline-session".into()),
                        params: json!({"task_id":killed_id}),
                    },
                    &kill_ctx,
                )
            }),
        )
        .await
        .expect("queued cancellation must not wait for admission")
        .unwrap();
        assert!(killed.success, "{killed:?}");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if daemon
                    .log
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(header, _)| header.ty == FrameType::Cancel)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the real client must send a call-key Cancel before receiving a job id");
        let log = daemon.log.lock().unwrap();
        let run = log
            .iter()
            .find(|(_, body)| body["method"] == "exec.run")
            .unwrap()
            .0;
        let cancel = log
            .iter()
            .find(|(header, _)| header.ty == FrameType::Cancel)
            .unwrap()
            .0;
        assert_eq!(
            (cancel.channel, cancel.epoch, cancel.corr),
            (run.channel, run.epoch, run.corr)
        );
        drop(log);
        daemon.acceptance_gate.notify_one();
        let status = admission_status(&ctx, &task_id);
        assert_eq!(
            status.data["status"], "fate_unknown",
            "unsubscribe is not terminal proof: {status:?}"
        );
        assert!(!status.data.to_string().contains("admitted"), "{status:?}");
        assert!(
            status
                .data
                .to_string()
                .contains(crate::bash_background::registry::remote::PRE_ACCEPTANCE_CANCELLED),
            "{status:?}"
        );
    }

    /// A runner that is slow to accept a job must not hold a foreground call
    /// past its foreground window. The call's `timeout` is ten minutes, so the
    /// wait for the runner's acceptance alone would last a full minute; only
    /// the one-second window may end it. The reply must carry the task id, say
    /// the job awaits admission and must not be rerun, and the job's later
    /// completion must arrive on that same task as a background completion.
    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_runon_replies_within_its_window_when_admission_is_slow() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::GatedAcceptanceThenExit).await;
        ctx.update_config(|config| config.foreground_wait_window_ms = 1_000);
        let received = Instant::now();
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "timeout":600_000}),
        );
        let done = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a foreground runon call must answer at its window, not at runner admission")
            .unwrap();
        let waited = received.elapsed();
        assert!(
            waited >= Duration::from_secs(1),
            "the foreground call replied early after {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "the reply took {waited:?}, past the one-second window"
        );
        let response = done.response_for_test();
        let text = done.text_for_test().unwrap();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap().to_owned();
        assert_eq!(response.data["status"], "starting", "{response:?}");
        assert_eq!(response.data["remote_phase"], "submitting", "{response:?}");
        assert_eq!(
            response.data["output"].as_str().unwrap(),
            format!(
                "Foreground bash didn't finish within 1s and was promoted to background: {task_id}. A completion reminder will be delivered automatically; use bash_status({{ taskId: \"{task_id}\" }}) to inspect output or bash_kill({{ taskId: \"{task_id}\" }}) to terminate.\n{REMOTE_ADMISSION_PENDING_NOTE}\n{REMOTE_INPUT_CAPTURE_WARNING}"
            ),
        );
        assert!(text.contains(&task_id), "{text}");
        assert!(text.contains(REMOTE_ADMISSION_PENDING_NOTE), "{text}");
        assert!(text.contains(REMOTE_INPUT_CAPTURE_WARNING), "{text}");
        assert!(
            ctx.bash_background()
                .remote_progress(&task_id, "deadline-session")
                .unwrap()
                .job_id
                .is_none(),
            "the reply must come before the runner accepted the job"
        );

        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "completed", |registry| {
            registry.is_task_terminal(&task_id)
        })
        .await;
        assert_eq!(
            ctx.bash_background()
                .observed_status(&task_id, "deadline-session", 0)
                .unwrap()
                .info
                .status,
            crate::bash_background::BgTaskStatus::Completed
        );
        wait_for_task_state(&ctx, "completion queued", |registry| {
            registry.has_completions_for_session(Some("deadline-session"))
        })
        .await;
        assert!(
            ctx.bash_background()
                .drain_completions_for_session(Some("deadline-session"))
                .iter()
                .any(|completion| completion.task_id == task_id),
            "the promoted task must deliver its completion on the task id the reply named"
        );
    }

    /// A remote job the runner accepts and finishes inside the foreground
    /// window answers inline with its result, exactly like a fast local
    /// command: no promotion, no capture warning, no background completion.
    #[cfg(unix)]
    #[tokio::test]
    async fn fast_foreground_runon_answers_inline_without_promotion() {
        use crate::exec_remote::wire_tests::Script;
        let (_daemon, _dir, root, executor, ctx) = capture_fixture(Script::Plain).await;
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "timeout":600_000}),
        );
        let done = tokio::time::timeout(Duration::from_secs(14), rx.recv())
            .await
            .expect("a fast remote job must answer inside the 15-second window")
            .unwrap();
        let response = done.response_for_test();
        let text = done.text_for_test().unwrap();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert!(!text.contains("promoted to background"), "{text}");
        assert!(!text.contains(REMOTE_ADMISSION_PENDING_NOTE), "{text}");
        assert!(!text.contains(REMOTE_INPUT_CAPTURE_WARNING), "{text}");
        let task_id = response.data["task_id"].as_str().unwrap();
        assert!(
            !ctx.bash_background()
                .drain_completions_for_session(Some("deadline-session"))
                .iter()
                .any(|completion| completion.task_id == task_id),
            "an inline result must not also arrive as a background completion"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_refusal_preserves_existing_error() {
        use crate::exec_remote::wire_tests::Script;
        let (_daemon, _dir, root, executor, ctx) = capture_fixture(Script::KnownRefused).await;
        let timing = BashTimingForTest::new(Instant::now());
        let (mut rx, task_id) = held_capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "timeout":600_000}),
            timing.clone(),
            capture_cancel(),
        )
        .await;
        wait_for_task_state(&ctx, "refusal saved before reply", |registry| {
            registry
                .runon_launch_state(&task_id, "deadline-session")
                .is_some_and(|launch| {
                    launch
                        .terminal
                        .is_some_and(|snapshot| snapshot.remote_refusal.is_some())
                })
        })
        .await;
        wait_for_task_state(&ctx, "canonical uncaptured drain reply", |registry| {
            registry
                .drain_reply_cache(&task_id, "deadline-session")
                .snapshot()
                .uncaptured_terminal
                .is_some()
        })
        .await;
        let expected = stateful_module_drain_reply_for_test(
            ctx.bash_background(),
            &task_id,
            "deadline-session",
            "cached-refusal",
            false,
            false,
        );
        let task = ctx.bash_background().task_for_test(&task_id).unwrap();
        {
            let _state = task.state.lock().unwrap();
            let _no_task_locks =
                crate::bash_background::registry::forbid_task_state_locks_for_test();
            let cached = ctx
                .bash_background()
                .drain_reply_cache(&task_id, "deadline-session")
                .snapshot();
            let terminal = cached.uncaptured_terminal.unwrap();
            assert_eq!(
                terminal.data, expected.data,
                "uncaptured drain must reuse the canonical terminal diagnostics"
            );
            assert_eq!(terminal.success, expected.success);
        }
        assert!(
            rx.try_recv().is_err(),
            "dispatch holds the reply until refusal is saved"
        );
        BashTimingForTest::release(&timing.result);
        let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "remote_unavailable");
        assert_eq!(response.data["message"], "runon refused: remote refused: unreachable; command was not run; retry, or omit runon to run locally; remote job 0192a64a-1234-7000-8000-000000000001");
        assert!(response.data["task_id"].as_str().is_some());
    }

    /// The runner connection drops before `Accepted`, so the worker settles
    /// the task as outcome unknown. A background call can already have returned its
    /// task ID with the pending-capture warning; a foreground call waits for and
    /// reports the saved outcome directly.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_connection_drop_before_accepted_reports_outcome_unknown() {
        use crate::exec_remote::wire_tests::Script;
        for background in [true, false] {
            let (daemon, _dir, root, executor, ctx) =
                capture_fixture(Script::DropBeforeAccepted).await;
            let mut rx = capture_call(
                &executor,
                &root,
                json!({"command":"uname -s", "runon":"linux", "background":background, "timeout":10000}),
            );
            daemon.wait_for_requests("exec.run", 1).await;
            let done = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("the launch must answer without waiting for the capture deadline")
                .unwrap();
            let response = done.response_for_test();
            let text = done.text_for_test().unwrap();
            let task_id = response.data["task_id"].as_str().unwrap().to_owned();
            if response.data["status"] == "starting" {
                assert!(
                    background,
                    "only a background call may hand off a live task"
                );
                assert_eq!(response.data["phase"], "submitting", "{response:?}");
                assert!(text.contains(REMOTE_ADMISSION_PENDING_NOTE), "{text}");
                assert!(text.contains(REMOTE_INPUT_CAPTURE_WARNING), "{text}");
            } else {
                assert_eq!(response.data["status"], "fate_unknown", "{response:?}");
                assert_ne!(response.data["phase"], "submitting", "{response:?}");
                assert!(!text.contains(REMOTE_INPUT_CAPTURE_WARNING), "{text}");
                assert!(
                text.contains("remote outcome unknown; the remote job could not affect this machine or the network, so rerunning is safe"),
                "{text}"
            );
            }
            wait_for_task_state(&ctx, "remote outcome unknown", |registry| {
                registry
                    .observed_status(&task_id, "deadline-session", 0)
                    .is_some_and(|snapshot| {
                        snapshot.info.status == crate::bash_background::BgTaskStatus::FateUnknown
                    })
            })
            .await;
            assert_eq!(
                ctx.bash_background()
                    .observed_status(&task_id, "deadline-session", 0)
                    .unwrap()
                    .info
                    .status,
                crate::bash_background::BgTaskStatus::FateUnknown
            );
        }
    }

    /// A ready reply finalized while its task looked live can turn out to
    /// describe a task that finished before the runner sent Accepted. Its
    /// replacement shows the saved outcome but keeps what finalization added:
    /// the trailing status text, the background completions it drained, and
    /// the formatted body as the text's prefix, so the repeat breaker still
    /// places its reminder before that trailing text. The finished result is
    /// acknowledged, as finalization does for a finished reply.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_ready_reply_swap_keeps_finalization() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, dir, root, executor, ctx) = capture_fixture(Script::DropBeforeAccepted).await;
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000}),
        );
        daemon.wait_for_requests("exec.run", 1).await;
        let launched = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let task_id = launched.response_for_test().data["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        wait_for_task_state(&ctx, "fate-unknown task", |registry| {
            registry
                .observed_status(&task_id, "deadline-session", 0)
                .is_some_and(|snapshot| {
                    snapshot.info.status == crate::bash_background::BgTaskStatus::FateUnknown
                })
        })
        .await;
        wait_for_task_state(&ctx, "background completion queued", |registry| {
            registry.has_completions_for_session(Some("deadline-session"))
        })
        .await;
        assert_eq!(
            ctx.bash_background()
                .observed_status(&task_id, "deadline-session", 0)
                .unwrap()
                .info
                .status,
            crate::bash_background::BgTaskStatus::FateUnknown
        );
        assert!(ctx
            .bash_background()
            .has_completions_for_session(Some("deadline-session")));

        let args = json!({"command":"uname -s"});
        let format_context =
            crate::subc_format::FormatContext::from_tool_call("bash", &args, dir.path());
        let completions = json!([{"task_id":"earlier-task", "status":"completed"}]);
        let live = Response::success(
            "ready-reply",
            json!({
                "output": "Command moved to the background",
                "task_id": task_id,
                "status": "running",
                "bg_completions": completions,
            }),
        );
        let additions = "[AFT E1 W2 | D0 U0 C0 | T0]";
        let live_text = format!(
            "{}\n\n{additions}",
            crate::subc_format::format_response_with_context("bash", &live, &format_context)
        );
        let swapped = recheck_runon_capture(
            ToolCallResult {
                text: live_text,
                response: live,
            },
            &ctx,
            &task_id,
            "deadline-session",
            &format_context,
        )
        .await;
        assert_eq!(
            swapped.response.data["status"], "fate_unknown",
            "{:?}",
            swapped.response
        );
        assert_eq!(swapped.response.data["bg_completions"], completions);
        let body = crate::subc_format::format_response_with_context(
            "bash",
            &swapped.response,
            &format_context,
        );
        assert!(swapped.text.starts_with(&body), "{}", swapped.text);
        assert!(swapped.text.ends_with(additions), "{}", swapped.text);
        assert!(
            swapped.text.contains("rerunning is safe"),
            "{}",
            swapped.text
        );
        assert!(!swapped.text.contains("moved to the background"));
        assert!(!swapped.text.contains(REMOTE_INPUT_CAPTURE_WARNING));
        assert!(
            !ctx.bash_background()
                .drain_completions_for_session(Some("deadline-session"))
                .iter()
                .any(|completion| completion.task_id == task_id),
            "the finished result was delivered in the reply"
        );

        // Two earlier identical calls, a minute ago, so this third one is steered
        // without the test waiting out the breaker's minimum span. They use
        // their own session: the launch above was already observed in
        // "deadline-session", and the breaker measures from that first call.
        let call = crate::response_finalize::repeat_breaker::RepeatCall::new("bash", &args);
        let earlier = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        for _ in 0..2 {
            ctx.repeat_breaker()
                .observe_at("repeat-session", &call, 0, earlier);
        }
        let repeat = crate::run_tool_call::RepeatObservation::for_agent_call(
            "repeat-session",
            "bash",
            &args,
            false,
            false,
            false,
        );
        let text = observe_completed_bash_result(swapped, &ctx, &format_context, repeat).text;
        let reminder = text
            .find("<system-reminder>")
            .unwrap_or_else(|| panic!("the third identical call must be steered: {text}"));
        assert!(text.starts_with(&body), "{text}");
        assert!(text.ends_with(additions), "{text}");
        assert!(reminder < text.len() - additions.len(), "{text}");
    }

    /// A background launch answers at task registration. If the runner has
    /// already accepted the command, its reply is the ordinary launch response;
    /// otherwise the task id and pending-capture warning arrive first. In either
    /// ordering, a later result belongs to the same task.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_accepted_then_completed_keeps_launch_reply() {
        use crate::exec_remote::wire_tests::Script;
        let args =
            json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000});

        // Keep the runner behind its gate until the pending launch reply is checked.
        let (daemon, _dir, root, executor, ctx) =
            capture_fixture(Script::GatedAcceptanceThenExit).await;
        let mut rx = capture_call(&executor, &root, args.clone());
        let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("an accepted launch must answer")
            .unwrap();
        let task_id = done.response_for_test().data["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            ctx.bash_background()
                .runon_inputs_captured(&task_id, "deadline-session"),
            Some(false),
            "the acceptance gate keeps the launch reply pending"
        );
        let response = done.response_for_test();
        let text = done.text_for_test().unwrap();
        assert_eq!(response.data["status"], "starting", "{response:?}");
        assert_eq!(response.data["remote_phase"], "submitting", "{response:?}");
        assert!(text.contains(REMOTE_ADMISSION_PENDING_NOTE), "{text}");
        assert!(text.contains(REMOTE_INPUT_CAPTURE_WARNING), "{text}");
        assert!(
            !ctx.bash_background().is_task_terminal(&task_id),
            "the runner has not accepted or completed the task"
        );
        daemon.acceptance_gate.notify_one();
        wait_for_task_state(&ctx, "runner accepted", |registry| {
            registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
        })
        .await;
        wait_for_task_state(&ctx, "completed", |registry| {
            registry.is_task_terminal(&task_id)
        })
        .await;
        assert_eq!(
            ctx.bash_background()
                .observed_status(&task_id, "deadline-session", 0)
                .unwrap()
                .info
                .status,
            crate::bash_background::BgTaskStatus::Completed
        );

        // The launch reply is decided only after the job has finished.
        let (_daemon, _dir, root, executor, ctx) = capture_fixture(Script::Plain).await;
        let timing = BashTimingForTest::new(Instant::now());
        let (mut rx, task_id) = held_capture_call(
            &executor,
            &root,
            args,
            timing.clone(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
        )
        .await;
        wait_for_task_state(&ctx, "runner accepted", |registry| {
            registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
        })
        .await;
        wait_for_task_state(&ctx, "completed", |registry| {
            registry.is_task_terminal(&task_id)
        })
        .await;
        assert!(rx.try_recv().is_err(), "the held dispatch holds the reply");
        BashTimingForTest::release(&timing.result);
        let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("the released launch must answer")
            .unwrap();
        // Acceptance and completion were observed before releasing the dispatch.
        assert_launch_reply(
            &done,
            &task_id,
            "reply after completion",
            ReplyDecided::AfterAcceptance,
        );
        assert_eq!(
            ctx.bash_background()
                .observed_status(&task_id, "deadline-session", 0)
                .unwrap()
                .info
                .status,
            crate::bash_background::BgTaskStatus::Completed
        );
    }

    /// A saturated blocking pool must not turn an already accepted launch back
    /// into a pending launch. The dispatch stays held while the reply is chosen.
    #[cfg(unix)]
    #[test]
    fn runon_capture_accepted_reply_does_not_queue_for_blocking_pool() {
        assert_capture_reply_without_blocking_pool(false);
    }

    #[cfg(unix)]
    #[test]
    fn runon_capture_refused_reply_does_not_queue_for_blocking_pool() {
        assert_capture_reply_without_blocking_pool(true);
    }

    #[cfg(unix)]
    fn assert_capture_reply_without_blocking_pool(refused: bool) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                use crate::exec_remote::wire_tests::Script;
                for (background, expire_read) in [(true, false), (false, false), (false, true)] {
                    let script = if refused { Script::AcceptedThenRefused } else { Script::Plain };
                    let (_daemon, _dir, root, executor, ctx) = capture_fixture(script).await;
                    let timing = BashTimingForTest::new(Instant::now());
                    let cancel = BashWaitCancel { speech_seed: None, run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(), };
                    let (mut rx, task_id) = held_capture_call(
                        &executor,
                        &root,
                        json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000}),
                        timing.clone(),
                        cancel.clone(),
                    )
                    .await;
                    wait_for_task_state(&ctx, "Accepted and terminal", |registry| {
                        registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
                            && registry.is_task_terminal(&task_id)
                    })
                    .await;

                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(60)).unwrap();
                    });
                    entered_rx.await.unwrap();
                    let task = ctx.bash_background().task_for_test(&task_id).unwrap();
                    let state = task.state.lock().unwrap();
                    let pending = with_capture_warning(&Response::success(
                        "queued-observation", json!({"task_id":task_id, "status":"running"}),
                    ));
                    let reply = await_runon_launch_reply(
                        ctx.bash_background().clone(), "deadline-session", &pending,
                        Some(if expire_read { Instant::now() - Duration::from_secs(2) } else { Instant::now() + Duration::from_secs(60) }), &cancel,
                        &drain::BashCallClaim::default(), false,
                        (!background && !expire_read).then(Instant::now), background,
                    ).await;
                    drop(state);
                    release_tx.send(()).unwrap();
                    blocker.await.unwrap();
                    BashTimingForTest::release(&timing.result);
                    let _ = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await.unwrap();
                    if !background && !refused {
                        assert!(reply.is_none(), "accepted foreground calls keep waiting: {reply:?}");
                        continue;
                    }
                    let reply = reply.unwrap();
                    if refused {
                        assert!(!reply.success, "a saved refusal must win: {reply:?}");
                        assert_eq!(reply.data["code"], "remote_unavailable");
                    } else {
                        assert_eq!(reply.data["status"], "running", "accepted launch: {reply:?}");
                        assert!(reply.data["remote_job_id"].is_string(), "{reply:?}");
                        assert_ne!(reply.data["remote_phase"], "submitting", "{reply:?}");
                    }
                    assert!(!reply.data.to_string().contains(REMOTE_INPUT_CAPTURE_WARNING), "{reply:?}");
                }
            });
    }

    /// This test checks launch replies after the input-capture wait ends once
    /// the runner has accepted the job. Hold dispatch and startup handoff until
    /// acceptance is observed, except for the explicitly gated pending case.
    /// A completed job still returns its task handle, not the finished result.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_wait_ended_after_accepted_keeps_launch_reply() {
        use crate::exec_remote::wire_tests::Script;
        for drain_ends_wait in [true, false] {
            for finished in [false, true] {
                let case = format!(
                    "{} after Accepted, job {}",
                    if drain_ends_wait {
                        "module drain"
                    } else {
                        "expired capture deadline"
                    },
                    if finished { "finished" } else { "running" }
                );
                let gate_acceptance_after_deadline = !drain_ends_wait && finished;
                let script = if gate_acceptance_after_deadline {
                    Script::GatedAcceptanceThenExit
                } else if finished {
                    Script::Plain
                } else {
                    Script::GatedStages { position: 1 }
                };
                let (daemon, _dir, root, executor, ctx) = capture_fixture(script).await;
                let mut timing = BashTimingForTest::new(Instant::now());
                if !drain_ends_wait {
                    Arc::get_mut(&mut timing).unwrap().capture_wait =
                        Some(Duration::from_millis(1));
                }
                let cancel = BashWaitCancel {
                    speech_seed: None,
                    run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(),
                };
                let received_at = if gate_acceptance_after_deadline {
                    Instant::now() - Duration::from_secs(2)
                } else {
                    Instant::now()
                };
                let (mut rx, task_id) = held_capture_call_with_metrics(
                &executor,
                &root,
                json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000}),
                timing.clone(),
                cancel.clone(),
                Arc::new(DispatchPathMetrics::new()),
                received_at,
            )
            .await;
                if gate_acceptance_after_deadline {
                    // Keep Accepted behind the runner gate until the 1 ms deadline reply is sent.
                    daemon.wait_for_requests("exec.run", 1).await;
                    assert_eq!(
                        ctx.bash_background()
                            .runon_inputs_captured(&task_id, "deadline-session"),
                        Some(false),
                        "the runner remains behind its acceptance gate after the capture deadline"
                    );
                    assert!(
                        Instant::now() >= received_at + Duration::from_millis(1),
                        "the one-millisecond capture deadline must expire before Accepted"
                    );
                    tokio::time::timeout(Duration::from_secs(5), timing.startup.armed.notified())
                        .await
                        .expect("startup handoff must be waiting for its test gate");
                    timing.startup.fire.notify_one();
                    let done = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                        .await
                        .expect("the expired capture deadline must answer before Accepted")
                        .unwrap();
                    assert_eq!(
                        ctx.bash_background()
                            .runon_inputs_captured(&task_id, "deadline-session"),
                        Some(false),
                        "the reply is selected while the runner is still gated"
                    );
                    daemon.acceptance_gate.notify_one();
                    wait_for_task_state(&ctx, "Accepted and completed", |registry| {
                        registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
                            && registry.is_task_terminal(&task_id)
                    })
                    .await;
                    BashTimingForTest::release(&timing.result);
                    // The reply was selected before acceptance, so its starting status is accurate.
                    assert_launch_reply(&done, &task_id, &case, ReplyDecided::BeforeAcceptance);
                    assert_eq!(done.response_for_test().data["status"], "starting");
                    continue;
                }

                wait_for_task_state(&ctx, "Accepted", |registry| {
                    registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
                        && (!finished || registry.is_task_terminal(&task_id))
                })
                .await;
                if drain_ends_wait {
                    cancel.drain.begin(Instant::now() + Duration::from_secs(60));
                }
                BashTimingForTest::release(&timing.result);
                let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
                    .await
                    .expect("the released launch must answer")
                    .unwrap();
                // Only the accepted cases reach this assertion. Their startup
                // timer and dispatch stayed held until Accepted was observed.
                assert_launch_reply(&done, &task_id, &case, ReplyDecided::AfterAcceptance);
                if !finished {
                    daemon.output_gate.notify_one();
                    wait_for_task_state(&ctx, "completed", |registry| {
                        daemon.completion_gate.notify_one();
                        registry.is_task_terminal(&task_id)
                    })
                    .await;
                }
            }
        }
    }

    /// A runner can accept a job (the inputs are captured) and then refuse it
    /// before it starts. The command never ran, so the launch reply must be
    /// that refusal error, not the ordinary `running` launch reply, however
    /// the capture wait ends: a module drain decides the reply without waiting
    /// for the worker to settle, and an expired capture deadline decides it
    /// from whatever state is saved by then. The remote handler is held until
    /// the refusal is recorded, so the reply is decided only afterwards.
    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_wait_ended_after_accepted_then_refused_reports_refusal() {
        use crate::exec_remote::wire_tests::Script;
        for drain_ends_wait in [true, false] {
            let case = if drain_ends_wait {
                "module drain after Accepted then refusal"
            } else {
                "expired capture deadline after Accepted then refusal"
            };
            let (_daemon, _dir, root, executor, ctx) =
                capture_fixture(Script::AcceptedThenRefused).await;
            let mut timing = BashTimingForTest::new(Instant::now());
            if !drain_ends_wait {
                // The capture deadline passes while the dispatch is held.
                Arc::get_mut(&mut timing).unwrap().capture_wait = Some(Duration::from_millis(1));
            }
            let cancel = BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            };
            let (mut rx, task_id) = held_capture_call(
                &executor,
                &root,
                json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":10000}),
                timing.clone(),
                cancel.clone(),
            )
            .await;
            wait_for_task_state(&ctx, "Accepted, then refused", |registry| {
                registry.runon_inputs_captured(&task_id, "deadline-session") == Some(true)
                    && registry
                        .observed_status(&task_id, "deadline-session", 0)
                        .is_some_and(|snapshot| snapshot.remote_refusal.is_some())
            })
            .await;
            if drain_ends_wait {
                cancel.drain.begin(Instant::now() + Duration::from_secs(60));
            }
            BashTimingForTest::release(&timing.result);
            let done = tokio::time::timeout(Duration::from_secs(60), rx.recv())
                .await
                .expect("the released launch must answer")
                .unwrap();
            let response = done.response_for_test();
            let text = done.text_for_test().unwrap();
            assert!(!response.success, "{case}: {response:?}");
            assert_eq!(
                response.data["code"], "remote_unavailable",
                "{case}: {response:?}"
            );
            assert!(
                response.data["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("command was not run")),
                "{case}: {response:?}"
            );
            assert_eq!(response.data["task_id"], task_id, "{case}: {response:?}");
            assert_ne!(response.data["status"], "running", "{case}: {response:?}");
            assert!(
                !text.contains(REMOTE_INPUT_CAPTURE_WARNING),
                "{case}: {text}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_replies_before_client_deadline() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, _ctx) = capture_fixture(Script::GatedAcceptance).await;
        let started = Instant::now();
        let budget = crate::commands::blocking_wait::ReplyBudget::new(
            started,
            Some(started + Duration::from_secs(2)),
        );
        let mut rx = with_bash_timing_for_test(Some(BashTimingForTest::new(started)), || {
            deadline_test_call_owned(
                &executor,
                &root,
                remote_runon_stub,
                json!({"command":"uname -s", "runon":"linux", "background":true, "timeout":600_000}),
                budget,
                false,
            )
        });
        // The launch reply does not depend on the runner receiving exec.run.
        // Measure its real client budget without first waiting for that event.
        let done = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        daemon.acceptance_gate.notify_one();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert!(response.data["task_id"].is_string(), "{response:?}");
        assert!(done
            .text_for_test()
            .unwrap()
            .contains(REMOTE_INPUT_CAPTURE_WARNING));
        assert!(
            started.elapsed() < Duration::from_millis(1700),
            "capture exceeded reply safety cutoff: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runon_capture_deadline_shares_timeout_and_warns_with_task_id() {
        use crate::exec_remote::wire_tests::Script;
        let (daemon, _dir, root, executor, ctx) = capture_fixture(Script::GatedAcceptance).await;
        let started = Instant::now();
        let mut rx = capture_call(
            &executor,
            &root,
            json!({"command":"uname -s", "runon":"linux", "timeout":500}),
        );
        daemon.wait_for_requests("exec.run", 1).await;
        let done = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "capture must share the call timeout, not add 60 seconds"
        );
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task = response.data["task_id"].as_str().unwrap();
        assert!(response.data["output"].as_str().unwrap().ends_with(
            "The runner hasn't confirmed the inputs were copied yet. Do not change or delete input files until bash_status shows the job queued or running."), "{response:?}");
        assert!(ctx
            .bash_background()
            .remote_progress(task, "deadline-session")
            .unwrap()
            .job_id
            .is_none());
        daemon.acceptance_gate.notify_one();
    }

    #[tokio::test]
    async fn local_background_reply_does_not_wait_for_remote_capture() {
        let (_dir, root) = super::super::test_support::test_root("local-no-capture-wait");
        let ctx = super::super::test_support::test_ctx();
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"sleep 120", "background":true, "timeout":120000}),
        );
        // The bound only has to separate "replied at launch" from "waited for
        // the command" (120 s); a tight bound failed on a loaded Windows runner.
        let done = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("a local background launch must not wait for capture or command completion")
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert!(!done.text_for_test().unwrap().contains("hasn't confirmed"));
        let task = response.data["task_id"].as_str().unwrap();
        assert!(!ctx
            .bash_background()
            .observed_status(task, "deadline-session", 0)
            .unwrap()
            .info
            .status
            .is_terminal());
        let _ = ctx.bash_background().kill(task, "deadline-session");
    }

    #[tokio::test]
    async fn bash_deadline_refuses_contended_admission_lock() {
        let (_dir, root) = super::super::test_support::test_root("bash-admission-lock");
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), super::super::test_support::test_ctx());
        let holder_executor = executor.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_executor.hold_state_lock_for_test(|| {
                held_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(350));
            })
        });
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"sleep 3"}),
        );
        let done = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        let elapsed = started.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(200),
            "admission must never park the frame runtime: {elapsed:?}"
        );
        assert_eq!(
            done.unwrap().unwrap().response_for_test().data["code"],
            "executor_busy"
        );
    }

    async fn deadline_queue_case(saturation: bool) {
        let (_dir, root) = super::super::test_support::test_root("bash-deadline");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 80;
            config.project_root = Some(root.as_path().into());
        });
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        executor.register_actor(root.clone(), ctx);
        let mut holders = Vec::new();
        // Use the effective pool size: the executor clamps a requested one-
        // worker pool to two. Bind-only reserve workers cannot run bash jobs.
        for index in 0..if saturation { executor.pool_size() } else { 1 } {
            let holder_root = if saturation {
                let path = root.as_path().join(format!("other-{index}"));
                std::fs::create_dir(&path).unwrap();
                let other = ProjectRootId::from_path(path).unwrap();
                executor.register_actor(other.clone(), super::super::test_support::test_ctx());
                other
            } else {
                root.clone()
            };
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = executor.submit_async(
                holder_root,
                Lane::Mutating,
                "long-writer".into(),
                Box::new(move |_| {
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(2));
                    Response::success("long-writer", json!({}))
                }),
            );
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            holders.push((release_tx, holder));
        }
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"printf ran > late-command", "timeout":60000}),
        );
        let done = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        for (release_tx, holder) in holders {
            release_tx.send(()).unwrap();
            holder.await.unwrap();
        }
        let response = done
            .expect("bash must refuse before the transport deadline")
            .unwrap();
        assert_eq!(
            response.response_for_test().data["code"],
            "bash_start_deadline"
        );
        let message = response.response_for_test().data["message"]
            .as_str()
            .unwrap();
        assert!(
            message.contains(if saturation {
                "workers saturated"
            } else {
                "root Mutating job long-writer"
            }),
            "{message}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !root.as_path().join("late-command").exists(),
            "a refused bash must never start later"
        );
    }

    #[tokio::test]
    async fn bash_deadline_refuses_behind_long_root_writer() {
        deadline_queue_case(false).await;
    }

    #[tokio::test]
    async fn bash_deadline_refuses_executor_saturation() {
        deadline_queue_case(true).await;
    }

    fn slow_start_stub(req: RawRequest, ctx: &AppContext) -> Response {
        std::thread::sleep(Duration::from_millis(300));
        running_bash_stub(req, ctx)
    }

    fn held_before_spawn_stub(req: RawRequest, ctx: &AppContext) -> Response {
        crate::bash_background::enter_bash_startup_step(
            crate::bash_background::BashStartupStep::Profile,
        );
        hold_startup_checkpoint();
        running_bash_stub(req, ctx)
    }

    fn disk_full_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(TaskIoFault::LayoutEnospc, || running_bash_stub(req, ctx))
    }

    fn running_disk_full_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(TaskIoFault::RunningHook(held_running_enospc), || {
            running_bash_stub(req, ctx)
        })
    }

    fn slow_running_write_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(TaskIoFault::RunningHook(held_running_write), || {
            running_bash_stub(req, ctx)
        })
    }

    async fn startup_case(
        dispatch: DispatchFn,
        window: u64,
    ) -> (ControlledStartup, BashDeferredCompletion) {
        let mut startup = ControlledStartup::new(
            dispatch,
            window,
            "printf ran > started-command",
            Instant::now(),
            false,
            false,
        );
        let done = startup.reply_at_checkpoint().await;
        (startup, done)
    }

    #[tokio::test]
    async fn bash_deadline_fences_slow_startup_before_process_creation() {
        let (mut startup, done) = startup_case(held_before_spawn_stub, 80).await;
        assert_eq!(done.response_for_test().data["code"], "bash_start_deadline");
        startup.finish().await;
        assert!(
            !startup.call.dir.path().join("started-command").exists(),
            "a startup refusal must fence late process creation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_deadline_warning_and_refusal_name_the_running_startup_step() {
        let (message, lines) = crate::logging::capture_log_lines(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread test runtime");
            runtime.block_on(async {
                // The test fires the deadline once the stub holds the sandbox
                // profile step. The window is only a backstop and must outlast
                // executor admission: at 80 ms a loaded macOS runner was still
                // admitting, so the backstop fired first and named admission.
                let (mut startup, done) = startup_case(held_before_spawn_stub, 3_000).await;
                let response = done.response_for_test();
                assert_eq!(response.data["code"], "bash_start_deadline");
                let message = response.data["message"]
                    .as_str()
                    .expect("startup refusal message")
                    .to_string();
                startup.finish().await;
                assert!(
                    !startup.call.dir.path().join("started-command").exists(),
                    "a refused startup must not start after the delayed step finishes"
                );
                message
            })
        });

        assert!(message.contains("sandbox profile build"), "{message}");
        let warning = lines
            .iter()
            .find(|line| line.contains("bash startup reply deadline"))
            .expect("startup deadline warning was captured");
        assert!(warning.starts_with("WARN "), "{warning}");
        assert!(warning.contains("steps="), "{warning}");
        assert!(
            warning.contains("profile:") && warning.contains("(running)"),
            "{warning}"
        );
    }

    #[tokio::test]
    async fn bash_deadline_names_enospc_during_task_directory_creation() {
        let (mut startup, done) = startup_case(disk_full_stub, 1500).await;
        let response = done.response_for_test();
        assert!(!response.success);
        assert!(
            response.data["message"]
                .as_str()
                .unwrap()
                .contains("No space left on device"),
            "{response:?}"
        );
        startup.finish().await;
        assert!(!startup.call.dir.path().join("started-command").exists());
    }

    #[tokio::test]
    async fn bash_deadline_retains_task_when_running_metadata_hits_enospc() {
        let (mut startup, done) = startup_case(running_disk_full_stub, 1000).await;
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().expect("live task id");
        startup.finish().await;
        assert!(
            startup
                .call
                .ctx
                .bash_background()
                .task_for_test(task_id)
                .is_some(),
            "running process must be registered despite ENOSPC"
        );
    }

    #[tokio::test]
    async fn bash_deadline_hands_off_before_running_metadata_write_settles() {
        let (mut startup, done) = startup_case(slow_running_write_stub, 1000).await;
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"]
            .as_str()
            .expect("committed task id");
        startup.finish().await;
        assert!(
            startup
                .call
                .ctx
                .bash_background()
                .task_for_test(task_id)
                .is_some(),
            "receipt must name the actual late-registered task"
        );
    }

    // --- Startup reply deadline: every reply says definitely whether the
    // command runs. Process creation commits through the call's spawn
    // receipt after the starting record and control/output files exist; the
    // deadline settles the same receipt under its lock. ---

    #[cfg(unix)]
    const DEADLINE_SESSION: &str = "deadline-session";

    // Only the Unix tests below read `dir` and `ctx`; the in-process rewrite
    // test that also runs on Windows needs just the reply channel.
    #[cfg_attr(not(unix), allow(dead_code))]
    struct StartupCall {
        dir: tempfile::TempDir,
        ctx: Arc<AppContext>,
        rx: mpsc::Receiver<BashDeferredCompletion>,
    }

    // Freeze receipt time and stop dispatch at a named write/commit boundary.
    // The timer cannot win admission just because a shell or the executor is slow.
    struct ControlledStartup {
        call: StartupCall,
        timing: Arc<BashTimingForTest>,
    }

    impl ControlledStartup {
        fn new(
            dispatch: DispatchFn,
            window: u64,
            command: &str,
            received_at: Instant,
            server_completion: bool,
            rewrite: bool,
        ) -> Self {
            let mut timing = BashTimingForTest::new(received_at);
            Arc::get_mut(&mut timing).unwrap().dispatch = Some(dispatch);
            let call = with_bash_timing_for_test(Some(timing.clone()), || {
                startup_call(
                    controlled_startup_dispatch,
                    window,
                    command,
                    received_at,
                    server_completion,
                    rewrite,
                )
            });
            Self { call, timing }
        }

        async fn reply_at_checkpoint(&mut self) -> BashDeferredCompletion {
            tokio::time::timeout(Duration::from_secs(20), async {
                tokio::select! {
                    _ = self.timing.checkpoint.notified() => {
                        self.timing.startup.armed.notified().await;
                        self.timing.startup.fire.notify_one();
                    }
                    // A layout failure or prompt rewrite has no held write.
                    _ = self.timing.finished.notified() => {}
                }
                self.call.rx.recv().await.expect("startup reply")
            })
            .await
            .expect("startup checkpoint/reply hang guard")
        }

        async fn finish(&mut self) {
            BashTimingForTest::release(&self.timing.result);
            // Channel closure joins the late dispatch and reply path. An empty
            // quiet window would not prove that a refused job cannot run later.
            let extra = tokio::time::timeout(Duration::from_secs(20), self.call.rx.recv())
                .await
                .expect("late startup dispatch hang guard");
            assert!(extra.is_none(), "only one terminal reply");
        }
    }

    impl Drop for ControlledStartup {
        fn drop(&mut self) {
            BashTimingForTest::release(&self.timing.result);
        }
    }

    fn controlled_startup_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
        let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
        let response = timing.dispatch.unwrap()(req, ctx);
        timing.finished.notify_one();
        response
    }

    fn hold_startup_checkpoint() {
        let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
        timing.checkpoint.notify_one();
        let _released = timing
            .result
            .1
            .wait_while(timing.result.0.lock().unwrap(), |released| !*released)
            .unwrap();
    }

    fn held_running_write() -> std::io::Result<()> {
        hold_startup_checkpoint();
        Ok(())
    }

    fn held_running_enospc() -> std::io::Result<()> {
        hold_startup_checkpoint();
        Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "No space left on device",
        ))
    }

    #[cfg(unix)]
    fn held_starting_record_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(TaskIoFault::StartingHook(hold_startup_checkpoint), || {
            running_bash_stub(req, ctx)
        })
    }

    #[cfg(unix)]
    async fn approved_startup_call(client_window: Duration) -> StartupCall {
        let (dir, root) = super::super::test_support::test_root("bash-approved-startup");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(root.as_path().into());
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let route = route_key(1, 1);
        // A first-party spawn principal isolates startup timing from the native
        // sandbox helper executable, which is not built by lib-only tests.
        let identity = super::super::test_support::route_identity(&root, DEADLINE_SESSION);
        // Backdate receipt instead of sleeping through the permission prompt.
        let received_at = Instant::now() - Duration::from_secs(25);
        let reply_budget = crate::commands::blocking_wait::ReplyBudget::new(
            received_at,
            Some(received_at + client_window),
        );
        let prompt_deadline = received_at + Duration::from_secs(60);
        assert!(Instant::now() < prompt_deadline);
        let mut pending = HashMap::from([(
            ReverseCorrKey { route, corr: 2 },
            PendingBashAsk {
                handles_progress: false,
                route,
                tool_corr: 1,
                tool_flags: control_flags(),
                tool_ver: PROTOCOL_VERSION,
                root: root.clone(),
                project_root: root.as_path().into(),
                session_id: identity.session.clone(),
                spawn_principal: identity.spawn_principal.clone(),
                edit_slot_survives: None,
                call_key: None,
                request_id: "approved-startup".into(),
                arguments: json!({
                    "command": "printf ran > started-command; printf approval-complete",
                    "timeout": 5000,
                    "wait": true,
                }),
                format_context: crate::subc_format::FormatContext::default(),
                cancel: BashWaitCancel {
                    speech_seed: None,
                    run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(),
                },
                grants: Vec::new(),
                repeat: None,
                worker_session: false,
                worker_preset: false,
                reply_budget,
                asked_at: received_at,
                expires_at: reply_budget.cap(prompt_deadline),
            },
        )]);
        let approval = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Response,
            control_flags(),
            route.channel,
            route.epoch,
            2,
            serde_json::to_vec(&json!({"decision": "allow"})).unwrap(),
        )
        .unwrap();
        let (writer_tx, _writer_rx) = mpsc::channel(8);
        let (completion_tx, rx) = mpsc::channel(8);
        let (touch_tx, _) = mpsc::channel(8);
        handle_bash_elicitation_reply(
            &writer_tx,
            &approval,
            false,
            &mut pending,
            &HashMap::from([(route, identity)]),
            &mut HashMap::new(),
            &executor,
            &Arc::new(Notify::new()),
            &completion_tx,
            &touch_tx,
            &Arc::new(DispatchPathMetrics::new()),
            &mut HashMap::new(),
            running_bash_stub,
        )
        .await
        .unwrap();
        assert!(pending.is_empty(), "approval must consume the pending ask");
        StartupCall { dir, ctx, rx }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_approval_after_25_seconds_starts_within_60_second_prompt_limit() {
        let mut call = approved_startup_call(Duration::from_secs(180)).await;
        let done = first_reply(&mut call, Duration::from_secs(10)).await;
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert!(response.data["output"]
            .as_str()
            .unwrap()
            .contains("approval-complete"));
        assert!(call.dir.path().join("started-command").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_approval_without_reply_budget_refuses_startup() {
        // A 48 s client window reserves half for reply delivery, leaving no
        // startup time when approval arrives at 25 s, even before prompt expiry.
        let mut call = approved_startup_call(Duration::from_secs(48)).await;
        let done = first_reply(&mut call, Duration::from_secs(10)).await;
        assert_not_started_refusal(done.response_for_test());
        assert_never_started(&call);
    }

    fn startup_call(
        dispatch: DispatchFn,
        window: u64,
        command: &str,
        received_at: Instant,
        server_completion: bool,
        rewrite: bool,
    ) -> StartupCall {
        let (dir, root) = super::super::test_support::test_root("bash-startup-outcome");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = window;
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
            config.experimental_bash_rewrite = rewrite;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let rx = deadline_test_call_owned(
            &executor,
            &root,
            dispatch,
            json!({"command": command, "timeout": 60000}),
            received_at,
            server_completion,
        );
        StartupCall { dir, ctx, rx }
    }

    async fn first_reply(call: &mut StartupCall, within: Duration) -> BashDeferredCompletion {
        tokio::time::timeout(within, call.rx.recv())
            .await
            .expect("bash must answer within its reply budget")
            .expect("bash completion channel open")
    }

    /// Task ids that have a record in the session's task store, on disk.
    #[cfg(unix)]
    fn task_records_on_disk(ctx: &AppContext) -> Vec<String> {
        let session_dir = crate::bash_background::persistence::session_tasks_dir(
            &crate::bash_background::task_storage_dir(ctx),
            DEADLINE_SESSION,
        );
        match crate::bash_background::persistence::discover_task_ids(&session_dir) {
            Ok((ids, _)) => ids,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("cannot list task records: {error}"),
        }
    }

    /// Asserts the call's command never ran and never will: no process side
    /// effect, no registered task, no task record on disk.
    #[cfg(unix)]
    fn assert_never_started(call: &StartupCall) {
        assert!(
            !call.dir.path().join("started-command").exists(),
            "a startup reported as not started must never run"
        );
        assert!(
            call.ctx.bash_background().list(0).is_empty(),
            "a refused startup must not register a task"
        );
        assert_eq!(
            task_records_on_disk(&call.ctx),
            Vec::<String>::new(),
            "a refused startup must not leave a task record"
        );
    }

    #[cfg(unix)]
    fn assert_not_started_refusal(response: &Response) {
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "bash_start_deadline", "{response:?}");
        let message = response.data["message"].as_str().unwrap();
        assert!(
            message.contains("command was not started"),
            "a refusal must say definitely that the command did not start: {message}"
        );
    }

    #[cfg(unix)]
    async fn wait_for_terminal(
        ctx: &AppContext,
        task_id: &str,
    ) -> crate::bash_background::BgTaskStatus {
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(snapshot) =
                ctx.bash_background()
                    .observed_status(task_id, DEADLINE_SESSION, 0)
            {
                if snapshot.info.status.is_terminal() {
                    return snapshot.info.status;
                }
            }
            assert!(Instant::now() < give_up, "task {task_id} never finished");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    fn slow_starting_record_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(
            TaskIoFault::StartingDelay(Duration::from_millis(300)),
            || running_bash_stub(req, ctx),
        )
    }

    #[cfg(unix)]
    fn slow_running_record_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(
            TaskIoFault::RunningDelay(Duration::from_millis(1200)),
            || running_bash_stub(req, ctx),
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocking_bash_startup_ignores_the_foreground_window() {
        for (blocking_flag, value, nested) in [
            ("wait", json!(true), false),
            ("block_to_completion", json!(true), false),
            ("wait", json!("true"), true),
            ("block_to_completion", json!(1), false),
        ] {
            let (_dir, root) = super::super::test_support::test_root("blocking-startup");
            let ctx = super::super::test_support::test_ctx();
            ctx.update_config(|config| {
                config.foreground_wait_window_ms = 500;
                config.project_root = Some(root.as_path().into());
                config.sandbox.profile.head = crate::config::SandboxMode::Off;
            });
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), ctx);
            let mut arguments = json!({"command": "printf startup-terminal", "timeout": 5000});
            arguments[blocking_flag] = value;
            if nested {
                arguments = json!({"params": arguments});
            }
            let mut replies =
                deadline_test_call(&executor, &root, slow_running_record_stub, arguments);
            let done = tokio::time::timeout(Duration::from_secs(5), replies.recv())
                .await
                .unwrap()
                .unwrap();
            let response = done.response_for_test();
            assert!(response.success, "{blocking_flag}: {response:?}");
            assert_eq!(
                response.data["status"], "completed",
                "{blocking_flag}: {response:?}"
            );
            assert!(response.data["output"]
                .as_str()
                .unwrap()
                .contains("startup-terminal"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocking_bash_startup_ignores_the_foreground_window_before_commit() {
        let (_dir, root) = super::super::test_support::test_root("blocking-precommit-startup");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 100;
            config.project_root = Some(root.as_path().into());
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let mut replies = deadline_test_call(
            &executor,
            &root,
            slow_starting_record_stub,
            json!({"command": "printf startup-terminal", "wait": true, "timeout": 5000}),
        );
        let done = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert!(response.data["output"]
            .as_str()
            .unwrap()
            .contains("startup-terminal"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocking_bash_startup_still_obeys_the_wait_cap() {
        let (_dir, root) = super::super::test_support::test_root("blocking-startup-cap");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 100;
            config.bash.worker_wait_max_ms = 500;
            config.project_root = Some(root.as_path().into());
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut replies = deadline_test_call(
            &executor,
            &root,
            slow_running_record_stub,
            json!({"command": "sleep 2", "wait": true, "timeout": 5000}),
        );
        let done = tokio::time::timeout(Duration::from_secs(1), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "running", "{response:?}");
        let output = response.data["output"].as_str().unwrap();
        assert!(output.contains("0.5s"), "{response:?}");
        assert!(output.contains("wait limit"), "{response:?}");
        assert!(
            !output.contains("Foreground bash didn't finish"),
            "{response:?}"
        );
        let task_id = response.data["task_id"].as_str().unwrap();
        assert_eq!(
            wait_for_terminal(&ctx, task_id).await,
            crate::bash_background::BgTaskStatus::Completed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocking_committed_startup_waits_past_the_admission_budget() {
        let (_dir, root) = super::super::test_support::test_root("blocking-committed-startup");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 100;
            config.bash.worker_wait_max_ms = 20_000;
            config.project_root = Some(root.as_path().into());
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        // Leave 500 ms to commit; the running-record delay crosses the separate
        // 15 s admission deadline while the blocking cap still has time left.
        let mut replies = deadline_test_call_received(
            &executor,
            &root,
            slow_running_record_stub,
            json!({"command": "printf committed-terminal", "wait": true, "timeout": 5000}),
            Instant::now() - Duration::from_millis(14_500),
        );
        let done = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert!(response.data["output"]
            .as_str()
            .unwrap()
            .contains("committed-terminal"));
    }

    #[cfg(unix)]
    fn terminal_bash_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let mut response = running_bash_stub(req, ctx);
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap().to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = ctx
                .bash_background()
                .observed_status(&task_id, DEADLINE_SESSION, 0)
                .unwrap();
            if snapshot.info.status.is_terminal() {
                // Deliver a terminal launch observation deterministically, rather
                // than racing the child against spawn-response construction.
                response.data["status"] = serde_json::to_value(snapshot.info.status).unwrap();
                return response;
            }
            assert!(Instant::now() < deadline, "task {task_id} did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_spawn_observation_keeps_foreground_output() {
        let (_dir, root) = super::super::test_support::test_root("terminal-spawn-output");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(root.as_path().into());
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let mut replies = deadline_test_call(
            &executor,
            &root,
            terminal_bash_stub,
            json!({"command": "printf terminal-spawn-output", "wait": true}),
        );
        let done = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert!(response.data["output"]
            .as_str()
            .expect("foreground reply includes output")
            .contains("terminal-spawn-output"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocking_bash_still_fences_expired_startup_admission() {
        let (dir, root) = super::super::test_support::test_root("blocking-expired-startup");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 100;
            config.bash.worker_wait_max_ms = 20_000;
            config.project_root = Some(root.as_path().into());
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let mut replies = deadline_test_call_received(
            &executor,
            &root,
            running_bash_stub,
            json!({"command": "printf ran > started-command", "wait": true, "timeout": 5000}),
            Instant::now() - Duration::from_millis(15_100),
        );
        let done = tokio::time::timeout(Duration::from_secs(1), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_not_started_refusal(done.response_for_test());
        assert!(!dir.path().join("started-command").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_deadline_during_control_file_write_says_not_started_and_never_starts() {
        let mut startup = ControlledStartup::new(
            held_starting_record_stub,
            80,
            "printf ran > started-command",
            Instant::now(),
            false,
            false,
        );
        let done = startup.reply_at_checkpoint().await;
        let response = done.response_for_test();
        assert_not_started_refusal(&response);
        assert!(
            response.data["message"]
                .as_str()
                .unwrap()
                .contains("before process creation committed"),
            "{response:?}"
        );
        startup.finish().await;
        assert_never_started(&startup.call);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_deadline_after_commit_hands_off_the_task_and_it_completes() {
        let mut startup = ControlledStartup::new(
            slow_running_write_stub,
            500,
            "printf ran > started-command",
            Instant::now(),
            false,
            false,
        );
        let done = startup.reply_at_checkpoint().await;
        let response = done.response_for_test();
        assert!(
            response.success,
            "a committed startup must be handed off: {response:?}"
        );
        assert_eq!(response.data["status"], "running");
        let task_id = response.data["task_id"]
            .as_str()
            .expect("handoff names the task")
            .to_string();
        assert!(
            response.data["output"].as_str().unwrap().contains(&task_id),
            "{response:?}"
        );
        startup.finish().await;
        assert_eq!(
            wait_for_terminal(&startup.call.ctx, &task_id).await,
            crate::bash_background::BgTaskStatus::Completed
        );
        assert!(startup.call.dir.path().join("started-command").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_deadline_at_the_commit_boundary_is_never_ambiguous() {
        use crate::bash_background::{
            with_spawn_commit_time_for_test, with_spawn_receipt, SpawnReceipt,
        };

        // Drive the real spawn path at exact clock positions, then settle its
        // receipt as the subc reply timer does. A delay sweep can miss the
        // commit-side refusal entirely when the timer wins every late case.
        for offset in [-1i32, 0, 1] {
            let (dir, root) = super::super::test_support::test_root("bash-commit-boundary");
            let ctx = super::super::test_support::test_ctx();
            ctx.update_config(|config| {
                config.project_root = Some(root.as_path().into());
                config.storage_dir = Some(dir.path().join("storage"));
                config.sandbox.profile.head = crate::config::SandboxMode::Off;
            });
            let (_tx, rx) = mpsc::channel(1);
            let call = StartupCall { dir, ctx, rx };
            let deadline = Instant::now();
            let now = match offset {
                -1 => deadline - Duration::from_nanos(1),
                0 => deadline,
                _ => deadline + Duration::from_nanos(1),
            };
            let receipt = Arc::new(SpawnReceipt::new(deadline));
            let response = with_spawn_receipt(receipt.clone(), || {
                with_spawn_commit_time_for_test(now, || {
                    running_bash_stub(
                        RawRequest {
                            id: "deadline-call".into(),
                            command: "bash".into(),
                            lsp_hints: None,
                            session_id: Some("deadline-session".into()),
                            params: json!({"command": "printf ran > started-command", "timeout": 60000}),
                        },
                        &call.ctx,
                    )
                })
            });
            assert_eq!(
                response.success,
                offset < 0,
                "offset {offset}: {response:?}"
            );
            let deadline_response = startup_deadline_response(
                "deadline-call",
                &receipt.expire(),
                200,
                None,
                false,
                false,
                false,
                &Executor::new(),
                &root,
                true,
                receipt.startup_step_snapshot().current(),
            );
            assert_eq!(deadline_response.success, response.success);
            if offset < 0 {
                let task_id = response.data["task_id"]
                    .as_str()
                    .expect("commit before deadline must return a task id")
                    .to_string();
                assert_eq!(deadline_response.data["task_id"], task_id);
                assert_eq!(
                    wait_for_terminal(&call.ctx, &task_id).await,
                    crate::bash_background::BgTaskStatus::Completed,
                    "offset {offset}"
                );
                assert!(
                    call.dir.path().join("started-command").exists(),
                    "handed-off task must have run"
                );
            } else {
                assert_not_started_refusal(&response);
                assert_not_started_refusal(&deadline_response);
                assert_never_started(&call);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_owned_bash_committed_at_deadline_is_outcome_unknown_with_task_id() {
        let mut startup = ControlledStartup::new(
            slow_running_write_stub,
            500,
            "printf ran > started-command",
            Instant::now() - Duration::from_millis(19_500),
            true,
            false,
        );
        let done = startup.reply_at_checkpoint().await;
        let response = done.response_for_test();
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "outcome_unknown_startup_deadline");
        let task_id = response.data["task_id"]
            .as_str()
            .expect("outcome-unknown names the started task");
        let message = response.data["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("bash_status {task_id}")),
            "{message}"
        );
        assert!(!message.contains("not started"), "{message}");
        startup.finish().await;
    }

    #[cfg(unix)]
    fn slow_rewrite_stub(req: RawRequest, ctx: &AppContext) -> Response {
        hold_startup_checkpoint();
        crate::bash_rewrite::try_rewrite_for_request(
            "echo fenced >> notes.txt",
            &req.id,
            req.session_id.as_deref(),
            &req.host_tool_names(),
            ctx,
            &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        )
        .expect("the append rewrite accepts this command")
    }

    #[cfg(unix)]
    fn prompt_rewrite_stub(req: RawRequest, ctx: &AppContext) -> Response {
        crate::bash_rewrite::try_rewrite_for_request(
            "echo fenced >> notes.txt",
            &req.id,
            req.session_id.as_deref(),
            &req.host_tool_names(),
            ctx,
            &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        )
        .expect("the append rewrite accepts this command")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_process_rewrite_is_fenced_like_process_creation() {
        // Control: inside the budget the rewrite runs and appends.
        let mut startup = ControlledStartup::new(
            prompt_rewrite_stub,
            2000,
            "unused",
            Instant::now(),
            false,
            true,
        );
        let done = startup.reply_at_checkpoint().await;
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        startup.finish().await;
        assert!(startup.call.dir.path().join("notes.txt").exists());

        // Past the budget the rewrite must not run once the caller was told
        // the command was not started.
        let mut startup =
            ControlledStartup::new(slow_rewrite_stub, 80, "unused", Instant::now(), false, true);
        let done = startup.reply_at_checkpoint().await;
        let response = done.response_for_test();
        assert_not_started_refusal(&response);
        startup.finish().await;
        assert!(
            !startup.call.dir.path().join("notes.txt").exists(),
            "a refused rewrite must never append"
        );
    }

    impl BashTimingForTest {
        fn new(commit_time: Instant) -> Arc<Self> {
            Arc::new(Self {
                commit_time,
                capture_wait: None,
                capture_registered: tokio::sync::Notify::new(),
                startup: BashTestDeadline::default(),
                inline_settle: BashTestDeadline::default(),
                reply: BashTestDeadline::default(),
                reply_uses_clock: false,
                committed: tokio::sync::Notify::new(),
                task_id: Default::default(),
                admission: Default::default(),
                result: Default::default(),
                dispatch: None,
                checkpoint: tokio::sync::Notify::new(),
                finished: tokio::sync::Notify::new(),
            })
        }

        fn release(gate: &(std::sync::Mutex<bool>, std::sync::Condvar)) {
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        }

        fn wait(gate: &(std::sync::Mutex<bool>, std::sync::Condvar), label: &str) {
            let (open, _) = gate
                .1
                .wait_timeout_while(
                    gate.0.lock().unwrap(),
                    Duration::from_millis(3000),
                    |open| !*open,
                )
                .unwrap();
            assert!(*open, "{label} gate was not released");
        }

        async fn event(event: &tokio::sync::Notify, label: &str) {
            tokio::time::timeout(Duration::from_millis(3000), event.notified())
                .await
                .unwrap_or_else(|_| panic!("{label} event was not observed"));
        }
    }

    fn gated_inline_stub(req: RawRequest, _ctx: &AppContext) -> Response {
        let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
        BashTimingForTest::wait(&timing.admission, "inline admission");
        crate::bash_background::commit_spawn_receipt_inline()
            .expect("commit before injected deadline");
        timing.committed.notify_one();
        BashTimingForTest::wait(&timing.result, "inline result");
        Response::success(req.id, json!({ "output": "inline done" }))
    }

    fn gated_running_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
        BashTimingForTest::wait(&timing.admission, "task admission");
        let response = running_bash_stub(req, ctx);
        assert!(response.success, "{response:?}");
        *timing.task_id.lock().unwrap() = Some(
            response.data["task_id"]
                .as_str()
                .expect("registered task id")
                .to_string(),
        );
        timing.committed.notify_one();
        response
    }

    fn gated_native_running_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let timing = BASH_TIMING_FOR_TEST.with(|slot| slot.borrow().clone().unwrap());
        BashTimingForTest::wait(&timing.admission, "task admission");
        #[cfg(not(windows))]
        let response = running_bash_stub(req, ctx);
        #[cfg(windows)]
        let response = crate::bash_background::spawn(
            &req.id,
            req.session(),
            "ping -n 60 127.0.0.1",
            crate::bash_background::BashShell::Powershell,
            PathBuf::from("powershell.exe"),
            None,
            None,
            crate::bash_background::HardKill::from_timeout_ms(None),
            ctx,
            false,
            false,
            false,
            false,
            24,
            80,
            Vec::new(),
            false,
            None,
            None,
        );
        assert!(response.success, "{response:?}");
        *timing.task_id.lock().unwrap() = Some(
            response.data["task_id"]
                .as_str()
                .expect("registered task id")
                .to_string(),
        );
        timing.committed.notify_one();
        response
    }

    #[tokio::test]
    async fn blocking_bash_deadline_handoff_preserves_wait_limit_guidance() {
        // Force the reply watchdog to win while the ordinary wait is blocked
        // on the executor and task lock, rather than depending on timer jitter.
        for worker_session in [true, false] {
            for mode in ["wait", "block_to_completion", "foreground"] {
                let (dir, root) = super::super::test_support::test_root("blocking-handoff");
                let ctx = super::super::test_support::test_ctx();
                ctx.update_config(|config| {
                    config.bash.worker_wait_max_ms = 1500;
                    config.foreground_wait_window_ms = 1500;
                    config.project_root = Some(root.as_path().into());
                    config.storage_dir = Some(dir.path().join("storage"));
                    config.sandbox.profile.head = crate::config::SandboxMode::Off;
                    config.sandbox.profile.worker = crate::config::SandboxMode::Off;
                });
                let executor = Arc::new(Executor::new());
                executor.register_actor(root.clone(), ctx.clone());
                let received_at = Instant::now();
                let timing = BashTimingForTest::new(received_at);
                let command = if cfg!(windows) {
                    "ping -n 60 127.0.0.1"
                } else {
                    "sleep 60"
                };
                let mut arguments = json!({"command": command, "foreground_orchestrate": true});
                if mode != "foreground" {
                    arguments[mode] = json!(true);
                }
                let mut rx = with_bash_timing_for_test(Some(timing.clone()), || {
                    deadline_test_call_controlled(
                        &executor,
                        &root,
                        gated_native_running_stub,
                        arguments,
                        received_at,
                        false,
                        worker_session,
                        Arc::new(DispatchPathMetrics::new()),
                        BashWaitCancel {
                            speech_seed: None,
                            run_scope: None,
                            scope: None,
                            late_results: false,
                            ledger_carrier: None,
                            connection: PersistentCancelSignal::new(),
                            route: PersistentCancelSignal::new(),
                            drain: drain::ModuleDrainWindow::default(),
                        },
                    )
                });
                BashTimingForTest::event(&timing.startup.armed, "startup deadline armed").await;
                BashTimingForTest::release(&timing.admission);
                tokio::select! {
                    biased;
                    done = rx.recv() => panic!("startup answered before registration: {:?}", done.unwrap().response_for_test()),
                    _ = BashTimingForTest::event(&timing.committed, "task registered") => {},
                }
                let task_id = timing.task_id.lock().unwrap().clone().unwrap();
                let task = ctx.bash_background().task_for_test(&task_id).unwrap();
                BashTimingForTest::event(&timing.reply.armed, "reply watchdog armed").await;
                let (held_tx, held_rx) = oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let holder = executor.submit_async(
                    root,
                    Lane::Mutating,
                    "held-writer".into(),
                    Box::new(move |_| {
                        let _state = task.state.lock().unwrap();
                        held_tx.send(()).unwrap();
                        let _ = release_rx.recv_timeout(Duration::from_secs(5));
                        Response::success("held-writer", json!({}))
                    }),
                );
                held_rx.await.unwrap();
                timing.reply.fire.notify_one();
                let done = tokio::time::timeout(Duration::from_millis(2300), rx.recv()).await;
                let _ = release_tx.send(());
                holder.await.unwrap();
                let _ = ctx.bash_background().kill(&task_id, "deadline-session");
                let done = done
                    .expect("reply must bypass the held writer and task lock")
                    .unwrap();
                let result = done.result.expect("deadline handoff response");
                assert!(result.response.success, "{:?}", result.response);
                assert_eq!(result.response.data["task_id"], task_id);
                assert_eq!(result.response.data["status"], "running");
                if mode == "foreground" {
                    assert!(
                        result.text.contains("was promoted to background"),
                        "{}",
                        result.text
                    );
                    assert!(
                        !result.text.contains("worker wait limit"),
                        "{}",
                        result.text
                    );
                    continue;
                }
                assert!(
                    result.text.contains("still running after 1.5s"),
                    "worker={worker_session} {mode}: {}",
                    result.text
                );
                assert!(result.text.contains("it was not killed"), "{}", result.text);
                assert!(
                    result.text.contains("AFT kills this task at "),
                    "{}",
                    result.text
                );
                assert!(
                    result.text.contains("its default background limit"),
                    "{}",
                    result.text
                );
                assert!(
                    !result.text.contains("was promoted to background"),
                    "{}",
                    result.text
                );
            }
        }
    }

    #[tokio::test]
    async fn committed_inline_rewrite_answers_with_its_result_or_outcome_unknown() {
        for settles in [true, false] {
            // Start with the 500 ms admission budget already elapsed. Freeze
            // the receipt's commit clock at request arrival and release the
            // startup timer only after the stub reports its inline commit;
            // executor scheduling speed must not decide the startup outcome.
            let received_at = Instant::now() - Duration::from_millis(1000);
            let timing = BashTimingForTest::new(received_at);
            let mut call = with_bash_timing_for_test(Some(timing.clone()), || {
                startup_call(gated_inline_stub, 500, "unused", received_at, false, false)
            });
            BashTimingForTest::event(&timing.startup.armed, "startup deadline armed").await;
            BashTimingForTest::release(&timing.admission);
            tokio::select! {
                biased;
                done = call.rx.recv() => panic!("startup answered before inline commit: {:?}", done.unwrap().response_for_test()),
                _ = BashTimingForTest::event(&timing.committed, "inline receipt committed") => {},
            }
            timing.startup.fire.notify_one();
            BashTimingForTest::event(&timing.inline_settle.armed, "inline settle entered").await;
            if settles {
                BashTimingForTest::release(&timing.result);
            } else {
                timing.inline_settle.fire.notify_one();
            }
            let done = first_reply(&mut call, Duration::from_millis(3000)).await;
            // Release the blocked stub even if the assertions below fail.
            BashTimingForTest::release(&timing.result);
            let response = done.response_for_test();
            if settles {
                assert!(
                    response.success,
                    "a settled rewrite returns its own reply: {response:?}"
                );
                assert_eq!(response.data["output"], "inline done");
            } else {
                assert!(!response.success, "{response:?}");
                assert_eq!(response.data["code"], "outcome_unknown_startup_deadline");
            }
        }
    }

    #[tokio::test]
    async fn bash_deadline_promotion_bypasses_root_writer_and_task_lock() {
        let (dir, root) = super::super::test_support::test_root("bash-poll-deadline");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 2000;
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"sleep 4", "timeout":60000}),
        );
        let started_by = Instant::now() + Duration::from_millis(1500);
        let task = loop {
            if let Some(snapshot) = ctx.bash_background().list(0).first() {
                break ctx
                    .bash_background()
                    .task_for_test(&snapshot.info.task_id)
                    .unwrap();
            }
            assert!(
                Instant::now() < started_by,
                "task startup must fit the test budget"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = executor.submit_async(
            root,
            Lane::Mutating,
            "writer-after-spawn".into(),
            Box::new(move |_| {
                let _state = task.state.lock().unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Response::success("writer-after-spawn", json!({}))
            }),
        );
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let done = tokio::time::timeout(Duration::from_millis(2300), rx.recv()).await;
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        let done = done
            .expect("promotion must bypass both executor and registry locks")
            .unwrap();
        assert!(done.response_for_test().success);
        assert!(done.response_for_test().data["task_id"].as_str().is_some());
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
                Ok(Some(_))
            ),
            "only one terminal reply"
        );
    }

    #[tokio::test]
    async fn bash_deadline_handoff_observes_repeat_while_task_lock_is_held() {
        let (dir, root) = super::super::test_support::test_root("bash-repeat-deadline");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 2000;
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
        });
        let arguments = json!({"command":"sleep 60", "timeout":60000});
        let call = crate::response_finalize::repeat_breaker::RepeatCall::new("bash", &arguments);
        // Seed prior calls with injected times instead of spending thirty
        // seconds waiting for the breaker's minimum observation span.
        let now = Instant::now();
        for (age, output) in [(32, "first task"), (16, "second task")] {
            assert!(ctx
                .repeat_breaker()
                .observe_at(
                    "deadline-session",
                    &call,
                    crate::response_finalize::repeat_breaker::output_hash(output),
                    now - Duration::from_secs(age),
                )
                .is_none());
        }
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        // Admission is deliberately later than the foreground deadline. The
        // test fires that deadline only after registration and lock acquisition,
        // so it exercises handoff under contention rather than scheduler speed.
        let received_at = Instant::now() - Duration::from_millis(3000);
        let timing = BashTimingForTest::new(received_at);
        let mut rx = with_bash_timing_for_test(Some(timing.clone()), || {
            deadline_test_call_received(
                &executor,
                &root,
                gated_running_stub,
                arguments,
                received_at,
            )
        });
        BashTimingForTest::event(&timing.startup.armed, "task startup deadline armed").await;
        BashTimingForTest::release(&timing.admission);
        tokio::select! {
            biased;
            done = rx.recv() => panic!("startup answered before task registration: {:?}", done.unwrap().response_for_test()),
            _ = BashTimingForTest::event(&timing.committed, "task registered") => {},
        }
        let task_id = timing.task_id.lock().unwrap().clone().unwrap();
        let task = ctx.bash_background().task_for_test(&task_id).unwrap();
        BashTimingForTest::event(&timing.reply.armed, "foreground deadline armed").await;
        let (held_tx, held_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = executor.submit_async(
            root,
            Lane::Mutating,
            "held-writer".into(),
            Box::new(move |_| {
                let _state = task.state.lock().unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Response::success("held-writer", json!({}))
            }),
        );
        tokio::time::timeout(Duration::from_secs(1), held_rx)
            .await
            .expect("writer must acquire task lock")
            .unwrap();
        timing.reply.fire.notify_one();
        let done = tokio::time::timeout(Duration::from_millis(2300), rx.recv()).await;
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        let done = done
            .expect("handoff must bypass the held writer and task lock")
            .unwrap();
        let result = done.result.expect("handoff response");
        let task_id = result.response.data["task_id"]
            .as_str()
            .expect("handoff task id");
        let _ = ctx.bash_background().kill(task_id, "deadline-session");
        assert!(
            result
                .text
                .contains("This is the 3rd call with the same arguments"),
            "deadline handoff must steer: {}",
            result.text
        );
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
                Ok(Some(_))
            ),
            "only one terminal reply"
        );
        let next = ctx
            .repeat_breaker()
            .observe_at("deadline-session", &call, 0, Instant::now())
            .expect("fourth call");
        assert_eq!(next.count, 4, "the deadline path observes exactly once");
    }

    impl Drop for DeferredWaitReleaseGuard {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::write(path, b"release");
            }
        }
    }

    #[tokio::test]
    async fn cap_many_deferred_bash_waits_leave_maintenance_admission_available() {
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 6,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(16);
        let (poll_touch_tx, mut poll_touch_rx) = mpsc::channel(16);
        let mut roots = Vec::new();

        for index in 0..5 {
            let (dir, root) = super::super::test_support::test_root(&format!("bash-wait-{index}"));
            executor.register_actor(root.clone(), super::super::test_support::test_ctx());
            roots.push((dir, root));
        }

        let releases = DeferredWaitReleaseGuard(
            roots
                .iter()
                .take(4)
                .map(|(_, root)| root.as_path().join("release"))
                .collect(),
        );
        for (index, (_, root)) in roots.iter().take(4).enumerate() {
            // Each command polls for its release file. Windows runs bash commands
            // through PowerShell, so the loop is written in its syntax there.
            #[cfg(windows)]
            let command = format!(
                "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 10 }}",
                releases.0[index].display()
            );
            #[cfg(not(windows))]
            let command = format!(
                "while [ ! -f '{}' ]; do sleep 0.01; done",
                releases.0[index].display()
            );
            let connection = PersistentCancelSignal::new();
            let route = PersistentCancelSignal::new();
            submit_deferred_bash(
                false,
                &executor,
                &completion_tx,
                &poll_touch_tx,
                &metrics,
                running_bash_stub,
                root.clone(),
                root.as_path().to_path_buf(),
                format!("session-{index}"),
                format!("wait-{index}"),
                RouteChannel {
                    channel: index as u16 + 1,
                    epoch: 1,
                },
                index as u64 + 1,
                Flags::new(false, Priority::Passive, false),
                PROTOCOL_VERSION,
                json!({
                    "command": command,
                    "wait": true,
                    "timeout": 60_000,
                }),
                crate::subc_format::FormatContext::default(),
                BashWaitCancel {
                    speech_seed: None,
                    run_scope: None,
                    scope: None,
                    late_results: false,
                    ledger_carrier: None,
                    connection,
                    route,
                    drain: drain::ModuleDrainWindow::default(),
                },
                BindTrust::FirstParty,
                crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                None,
                None,
                None,
                None,
                None,
                false,
                false,
                false,
                false,
                super::remote_policy::RemoteSource::None,
                Instant::now(),
            );
        }

        let waits_started_by = Instant::now() + Duration::from_secs(60);
        while metrics
            .deferred_bash_waits_in_flight
            .load(Ordering::Relaxed)
            < 4
        {
            assert!(
                Instant::now() < waits_started_by,
                "all deferred bash waits should park off executor workers"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Every caller keeps its tool call open until the command actually exits.
        assert!(completion_rx.try_recv().is_err());
        assert_eq!(
            executor
                .try_dispatch_liveness_snapshot()
                .expect("dispatch liveness")
                .running
                .maintenance,
            0,
            "parked waits must not hold maintenance workers"
        );

        let admitted = executor.submit_maintenance_async(
            roots[4].1.clone(),
            Lane::MaintenanceCommit,
            "fresh-configure-tail".to_string(),
            Box::new(|_| {
                std::thread::sleep(Duration::from_millis(150));
                Response::success("fresh-configure-tail", json!({ "drained": true }))
            }),
        );
        let response = tokio::time::timeout(Duration::from_secs(60), admitted)
            .await
            .expect("maintenance admission must not wait for deferred bash")
            .expect("executor maintenance response");
        assert!(response.success);
        let maintenance_released_by = Instant::now() + Duration::from_secs(60);
        loop {
            let running = executor
                .try_dispatch_liveness_snapshot()
                .expect("dispatch liveness")
                .running
                .maintenance;
            if running == 0 {
                break;
            }
            assert!(
                Instant::now() < maintenance_released_by,
                "configure tail must release its maintenance slot promptly"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Maintenance must complete while the commands are still held, not
        // merely finish faster than a wall-clock threshold on an idle machine.
        assert!(completion_rx.try_recv().is_err());
        assert_eq!(
            metrics
                .deferred_bash_waits_in_flight
                .load(Ordering::Relaxed),
            4
        );
        drop(releases);
        // Require each long command's ordinary terminal response and exact formatted text.
        for _ in 0..4 {
            let completion = tokio::time::timeout(Duration::from_secs(60), completion_rx.recv())
                .await
                .expect("deferred completion deadline")
                .expect("deferred completion");
            let result = completion.result.expect("terminal bash result");
            assert!(result.response.success);
            // No output: the reply is only the line naming the task.
            assert_eq!(
                result.text,
                format!(
                    "[task {}]",
                    result.response.data["task_id"].as_str().unwrap()
                )
            );
            tokio::time::timeout(Duration::from_secs(60), poll_touch_rx.recv())
                .await
                .expect("poll touch deadline")
                .expect("poll touch");
        }
        assert_eq!(
            metrics
                .deferred_bash_waits_in_flight
                .load(Ordering::Relaxed),
            0
        );
    }

    /// A deferred bash wait runs on the frame loop's single-threaded runtime.
    /// Reject task-state lock acquisitions on that thread, even if uncontended:
    /// another thread may be persisting JSON or spawning a fallback under the
    /// same lock. Scheduler delays are not evidence of a lock acquisition.
    #[tokio::test]
    async fn deferred_bash_wait_does_not_block_the_frame_loop_on_task_state_lock() {
        assert_deferred_wait_entrypoint("sleep 2", None).await;
    }

    pub(super) async fn assert_deferred_wait_entrypoint(
        command: &str,
        release: Option<&std::path::Path>,
    ) {
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(4);
        let (poll_touch_tx, _poll_touch_rx) = mpsc::channel(16);
        let (_dir, root) = super::super::test_support::test_root("bash-wait-lock");
        executor.register_actor(root.clone(), super::super::test_support::test_ctx());
        let hang_timeout = Duration::from_secs(120);
        let _no_task_locks = crate::bash_background::registry::forbid_task_state_locks_for_test();

        submit_deferred_bash(
            false,
            &executor,
            &completion_tx,
            &poll_touch_tx,
            &metrics,
            running_bash_stub,
            root.clone(),
            root.as_path().to_path_buf(),
            "session-lock".to_string(),
            "wait-lock".to_string(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Passive, false),
            PROTOCOL_VERSION,
            json!({
                "command": command,
                "wait": true,
                // Only catch hangs: loaded Windows runners can spend the old
                // 10-second budget starting Git Bash, before `sleep 2` runs.
                // Frame-loop responsiveness is checked independently below.
                "timeout": hang_timeout.as_millis() as u64,
            }),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            super::remote_policy::RemoteSource::None,
            Instant::now(),
        );

        drop(completion_tx);
        // Require a real deferred wait before releasing a gated command. Its
        // terminal reply below also requires the wait's observation/poll path.
        tokio::time::timeout(hang_timeout, async {
            while metrics
                .deferred_bash_waits_in_flight
                .load(Ordering::Relaxed)
                < 1
            {
                tokio::select! {
                    _ = completion_rx.recv() => panic!(
                        "bash completed before the deferred wait entered"
                    ),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        })
        .await
        .expect("bash startup hung before the deferred wait entered");
        if let Some(release) = release {
            std::fs::write(release, b"release").unwrap();
        }

        // Wait for the completion event, not a second deadline that also charges
        // Windows shell startup and persistence against the frame-loop test.
        // The command's hard kill still bounds execution; require a normal exit
        // below so a hung command cannot pass merely by timing out.
        let completion = completion_rx.recv().await.expect("deferred completion");
        let response = completion.result.expect("terminal bash result").response;
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert_eq!(response.data["exit_code"], 0, "{response:?}");
    }
}

#[cfg(all(test, unix))]
mod run_tests {
    use super::*;

    async fn run(command: &str, timeout: u64, deadline: Option<Instant>) -> Value {
        run_with_scope(
            command,
            timeout,
            super::super::run_scope::RunScope::for_test(deadline),
            1,
            super::grant_path_tests::running_bash_stub,
        )
        .await
        .data
    }

    async fn run_with_scope(
        command: &str,
        timeout: u64,
        scope: Arc<super::super::run_scope::RunScope>,
        expected_tasks: usize,
        dispatch: DispatchFn,
    ) -> Response {
        let (_dir, root) = super::super::test_support::test_root("run-bash-lifetime");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| config.foreground_wait_window_ms = 10);
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(8);
        let (touch_tx, _touch_rx) = mpsc::channel(8);
        submit_deferred_bash(
            false,
            &executor,
            &completion_tx,
            &touch_tx,
            &metrics,
            dispatch,
            root.clone(),
            root.as_path().into(),
            "run-session".into(),
            "run-call".into(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            control_flags(),
            PROTOCOL_VERSION,
            json!({"command":command,"timeout":timeout}),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                speech_seed: None,
                run_scope: Some(scope),
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            super::super::remote_policy::RemoteSource::None,
            Instant::now(),
        );
        let done = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let response = done.response_for_test();
        let ctx = executor.actor_context(&root).unwrap();
        let tasks = ctx.bash_background().list(0);
        assert_eq!(tasks.len(), expected_tasks, "{response:?}");
        for task in &tasks {
            assert!(
                task.info.status.is_terminal(),
                "no background promotion: {task:?}"
            );
        }
        if let Some(task) = response.data.get("task_id").and_then(Value::as_str) {
            if let Some(path) = ctx.bash_background().task_json_path(task, "run-session") {
                let persisted: Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                assert_eq!(
                    persisted["notify_on_completion"], false,
                    "promotion enables background completion delivery: {persisted}"
                );
            }
        }
        copy_response(response)
    }

    #[tokio::test]
    async fn run_scoped_foreground_timeout_never_promotes() {
        let response = run("sleep 30", 150, None).await;
        assert_eq!(response["status"], "timed_out", "{response:?}");
        assert_eq!(response["exit_code"], 124);
    }

    #[tokio::test]
    async fn run_scoped_foreground_outlives_wait_window_without_promotion() {
        let started = Instant::now();
        let response = run("sleep 0.2; printf done", 5000, None).await;
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(response["status"], "completed", "{response:?}");
    }

    #[tokio::test]
    async fn run_available_budget_reaches_spawn_and_kills_at_deadline() {
        // Keep 200 ms available when the executor constrains the command,
        // regardless of queue time. The real child must receive that timeout
        // and return exit 124 instead of being promoted or running for 5 s.
        let started = Instant::now();
        let scope = super::super::run_scope::RunScope::for_test_with_clock(
            Some(started + Duration::from_millis(200)),
            Some(started),
        );
        let response = run_with_scope("sleep 30", 5000, scope, 1, budgeted_bash_stub).await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "timed_out", "{response:?}");
        assert_eq!(response.data["exit_code"], 124);
    }

    #[tokio::test]
    async fn run_budget_expired_before_spawn_refuses_without_a_task() {
        let deadline = Instant::now() + Duration::from_millis(200);
        let scope =
            super::super::run_scope::RunScope::for_test_with_clock(Some(deadline), Some(deadline));
        let response = run_with_scope("sleep 30", 5000, scope, 0, budgeted_bash_stub).await;
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "run_deadline_expired");
        assert_eq!(
            response.data["message"],
            "the codemode run's time budget has expired"
        );
        assert!(response.data.get("status").is_none(), "{response:?}");
        assert!(response.data.get("exit_code").is_none(), "{response:?}");
    }

    fn budgeted_bash_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let params = req.params.get("params").unwrap_or(&req.params);
        assert_eq!(
            params["timeout"], 200,
            "spawn must receive the available run budget"
        );
        super::grant_path_tests::running_bash_stub(req, ctx)
    }
}
