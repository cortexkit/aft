//! Standalone read admission shares the supervised transport's executor lanes.

use aft::context::AppContext;
use aft::executor::{Executor, ExecutorConfig, JobCancellation, Lane};
use aft::path_identity::ProjectRootId;
use aft::protocol::{RawRequest, Response};
use aft::response_finalize::{DispatchOutcome, PendingResponse};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Separate admission budgets keep a burst of graph queries from refusing reads.
const LANE_CAP: usize = 256;

pub(super) struct StandaloneReads {
    executor: Arc<Executor>,
    reads: Arc<AtomicUsize>,
    heavy: Arc<AtomicUsize>,
    #[cfg(test)]
    probe: Option<Arc<dyn Fn(&RawRequest) + Send + Sync>>,
}

impl StandaloneReads {
    pub(super) fn new() -> Self {
        // Even on small machines, a capped heavy lane must leave an actor and
        // worker slot for a cheap read. The executor still owns all scheduling.
        let mut config = ExecutorConfig::default();
        config.pool_size = config.pool_size.max(3);
        config.actor_cap = config.actor_cap.max(2);
        config.heavy_permits = config.heavy_permits.min(config.actor_cap - 1);
        let executor = Arc::new(Executor::with_config(config));
        Self {
            executor,
            reads: Arc::new(AtomicUsize::new(0)),
            heavy: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            probe: None,
        }
    }

    pub(super) fn dispatch(
        &self,
        req: RawRequest,
        ctx: &Arc<AppContext>,
        lane: Lane,
    ) -> DispatchOutcome {
        let config = ctx.config();
        let root = match ProjectRootId::from_path(
            config
                .project_root
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new(".")),
        ) {
            Ok(root) => root,
            Err(error) => {
                return DispatchOutcome::Immediate(Response::error(
                    &req.id,
                    "invalid_project_root",
                    format!("standalone root identity: {error}"),
                ))
            }
        };
        self.executor.register_actor(root.clone(), Arc::clone(ctx));
        let count = match lane {
            Lane::PureRead => &self.reads,
            Lane::HeavyInit => &self.heavy,
            _ => unreachable!("standalone parallel read lane"),
        };
        let Some(slot) = AdmissionSlot::try_acquire(count) else {
            return DispatchOutcome::Immediate(Response::error_with_data(
                &req.id,
                "busy",
                "standalone read lane is full; retry after outstanding calls finish",
                serde_json::json!({"retryable": true}),
            ));
        };
        let id = req.id.clone();
        let session = req.session().to_string();
        let command = super::attach_command_for_request(&req);
        let cancellation = JobCancellation::new();
        let deadline = req
            .params
            .get("deadline_ms")
            .and_then(|v| v.as_u64())
            .map(|ms| {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let remaining = u128::from(ms)
                    .saturating_sub(now_ms)
                    .min(u128::from(u64::MAX)) as u64;
                Instant::now() + Duration::from_millis(remaining)
            });
        let (tx, rx) = mpsc::sync_channel(1);
        let ask_route = aft::access_policy::capture_ask_route();
        let worker_session = req.session_id.clone();
        let worker_ctx = Arc::clone(ctx);
        #[cfg(test)]
        let probe = self.probe.clone();
        self.executor.submit_tool_call_with_completion(
            root.clone(),
            lane,
            id.clone(),
            &command,
            Box::new(move |ctx| {
                // Detached view updates must share the process-wide publication
                // lock used by standalone mutations and maintenance. The executor's
                // actor lock alone does not protect those inline file mutations.
                let _publication = aft::executor::install_standalone_scope(worker_ctx);
                let _ask_route = ask_route.install();
                let _config = ctx.pin_config_to(config);
                if expired(deadline) {
                    return deadline_response(&req.id);
                }
                #[cfg(test)]
                if let Some(probe) = probe {
                    probe(&req);
                }
                aft::log_ctx::with_session(worker_session, || super::dispatch(req, ctx))
            }),
            cancellation.clone(),
            Box::new(move |response| {
                drop(slot);
                let _ = tx.send(response);
                super::wake_standalone_loop_for_deferred_response();
            }),
        );
        let poll_id = id.clone();
        let shutdown_id = id.clone();
        let shutdown_command = command.clone();
        let executor = self.executor.clone();
        let poll_cancellation = cancellation.clone();
        DispatchOutcome::Deferred(
            PendingResponse::polling(
                id,
                session,
                command,
                Box::new(move |_| {
                    if poll_cancellation.cancel_already_requested() {
                        // Queued jobs settle immediately; running handlers own
                        // their terminal response, including any partial data.
                        executor.cancel_job(&root, &poll_cancellation);
                    }
                    match rx.try_recv() {
                        Ok(response) => Some(response),
                        Err(mpsc::TryRecvError::Disconnected) => Some(Response::error(
                            &poll_id,
                            "internal_error",
                            "standalone read completion disconnected",
                        )),
                        Err(mpsc::TryRecvError::Empty) => {
                            if expired(deadline) {
                                executor.cancel_job(&root, &poll_cancellation);
                                Some(deadline_response(&poll_id))
                            } else {
                                None
                            }
                        }
                    }
                }),
            )
            .with_poll_interval(Duration::from_millis(10))
            .with_cancellation(cancellation)
            .with_shutdown(Box::new(move |_| {
                super::cancelled_deferred_response(&shutdown_id, &shutdown_command)
            })),
        )
    }
}

struct AdmissionSlot(Arc<AtomicUsize>);

impl AdmissionSlot {
    fn try_acquire(count: &Arc<AtomicUsize>) -> Option<Self> {
        let mut observed = count.load(Ordering::Acquire);
        loop {
            if observed >= LANE_CAP {
                return None;
            }
            match count.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(Arc::clone(count))),
                Err(current) => observed = current,
            }
        }
    }
}

impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn deadline_response(id: &str) -> Response {
    Response::error(id, "deadline_exceeded", "standalone read deadline expired")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft::config::Config;
    use aft::response_finalize::PendingResponses;
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    fn fixture(root: &std::path::Path) -> Arc<AppContext> {
        Arc::new(
            AppContext::new(
                aft::context::default_language_provider_factory(),
                crate::context_storage::isolate(Config {
                    project_root: Some(root.to_path_buf()),
                    format_on_edit: false,
                    ..Config::default()
                }),
            )
            .with_test_checkpoint_store(),
        )
    }

    fn request(id: &str, command: &str, params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: id.into(),
            command: command.into(),
            session_id: Some("ordered".into()),
            lsp_hints: None,
            params,
        }
    }

    fn register(pending: &mut PendingResponses, outcome: DispatchOutcome) {
        let DispatchOutcome::Deferred(response) = outcome else {
            panic!("parallel read must not execute on the frame loop")
        };
        pending
            .register(response)
            .unwrap_or_else(|_| panic!("unique id"));
    }

    fn finish(ctx: &AppContext, pending: &mut PendingResponses) -> Vec<Response> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut responses = Vec::new();
        while !pending.is_empty() {
            responses.extend(pending.poll_ready(ctx).into_iter().map(|r| r.response));
            assert!(
                Instant::now() < deadline,
                "standalone reads did not complete"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        responses
    }

    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }

    #[test]
    fn standalone_slow_callgraph_burst_does_not_delay_zoom_stage_timestamp() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("fixture.rs"), "fn fast_zoom() {}\n").unwrap();
        let ctx = fixture(root.path());
        let mut reads = StandaloneReads::new();
        let clock_ms = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(Arc::clone(&gate));
        let (stage_tx, stage_rx) = mpsc::channel();
        let (slow_tx, slow_rx) = mpsc::channel();
        let clock = Arc::clone(&clock_ms);
        reads.probe = Some(Arc::new(move |req| {
            if req.id.starts_with("slow-") {
                slow_tx.send(()).unwrap();
                let mut released = gate.0.lock().unwrap();
                while !*released {
                    released = gate.1.wait(released).unwrap();
                }
                // Inject handler cost into a logical clock, not host scheduling
                // time. A zoom behind even one slow handler exceeds the budget.
                clock.fetch_add(500, Ordering::SeqCst);
            } else if req.id == "fast" {
                stage_tx.send(clock.load(Ordering::SeqCst)).unwrap();
            }
        }));
        let mut pending = PendingResponses::default();
        for n in 0..12 {
            register(
                &mut pending,
                super::super::dispatch_outcome(
                    request(
                        &format!("slow-{n}"),
                        "callers",
                        serde_json::json!({"file":"fixture.rs", "symbol":"fast_zoom"}),
                    ),
                    &ctx,
                    &reads,
                ),
            );
        }
        for _ in 0..reads.executor.heavy_permits() {
            slow_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        let received_ms = clock_ms.load(Ordering::SeqCst);
        register(
            &mut pending,
            super::super::dispatch_outcome(
                request(
                    "fast",
                    "zoom",
                    serde_json::json!({"file":"fixture.rs", "symbol":"fast_zoom"}),
                ),
                &ctx,
                &reads,
            ),
        );
        let selected_ms = stage_rx.recv_timeout(Duration::from_secs(10));
        drop(release);
        let responses = finish(&ctx, &mut pending);
        assert!(responses.iter().find(|r| r.id == "fast").unwrap().success);
        assert!(
            selected_ms.expect("zoom starts before heavy handlers are released") - received_ms < 10,
            "zoom must enter its handler within a 10 ms injected-stage budget"
        );
    }

    #[test]
    fn standalone_write_then_parallel_reads_observe_mutation_in_same_session() {
        let root = tempfile::tempdir().unwrap();
        let ctx = fixture(root.path());
        let reads = StandaloneReads::new();
        for wrapped in [false, true] {
            let symbol = if wrapped {
                "after_wrapped_write"
            } else {
                "after_bare_write"
            };
            let outcome = super::super::dispatch_outcome(
                request(
                    "write",
                    "write",
                    serde_json::json!({
                        "file":"fixture.rs", "content":format!("fn {symbol}() {{}}\n"), "validate":"off"
                    }),
                ),
                &ctx,
                &reads,
            );
            let DispatchOutcome::Immediate(written) = outcome else {
                panic!("write prefix must complete")
            };
            assert!(written.success, "{written:?}");
            let mut pending = PendingResponses::default();
            for (command, params) in [
                (
                    "grep",
                    serde_json::json!({"pattern":symbol, "path":"fixture.rs"}),
                ),
                (
                    "outline",
                    if wrapped {
                        serde_json::json!({"target":"fixture.rs"})
                    } else {
                        serde_json::json!({"file":"fixture.rs"})
                    },
                ),
                (
                    "zoom",
                    if wrapped {
                        serde_json::json!({"batch":[{"target":"fixture.rs", "symbols":symbol}]})
                    } else {
                        serde_json::json!({"file":"fixture.rs", "symbol":symbol})
                    },
                ),
            ] {
                let req = if wrapped {
                    request(
                        command,
                        "tool_call",
                        serde_json::json!({"name":command, "arguments":params}),
                    )
                } else {
                    request(command, command, params)
                };
                register(
                    &mut pending,
                    super::super::dispatch_outcome(req, &ctx, &reads),
                );
            }
            for response in finish(&ctx, &mut pending) {
                assert!(response.success, "{response:?}");
                assert!(response.data.to_string().contains(symbol), "{response:?}");
            }
        }
    }

    #[test]
    fn standalone_parallel_read_admission_is_bounded_cancellable_and_deadlined() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("fixture.rs"), "fn fast_zoom() {}\n").unwrap();
        let ctx = fixture(root.path());
        let mut reads = StandaloneReads::new();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(Arc::clone(&gate));
        reads.probe = Some(Arc::new(move |req| {
            if req.command == "callers" {
                let mut released = gate.0.lock().unwrap();
                while !*released {
                    released = gate.1.wait(released).unwrap();
                }
            }
        }));
        let mut pending = PendingResponses::default();
        for n in 0..LANE_CAP {
            register(
                &mut pending,
                super::super::dispatch_outcome(
                    request(
                        &format!("queued-{n}"),
                        "callers",
                        serde_json::json!({"file":"fixture.rs", "symbol":"fast_zoom"}),
                    ),
                    &ctx,
                    &reads,
                ),
            );
        }
        let DispatchOutcome::Immediate(busy) = super::super::dispatch_outcome(
            request("overflow", "callers", serde_json::json!({})),
            &ctx,
            &reads,
        ) else {
            panic!("full lane must refuse without queuing")
        };
        assert_eq!(busy.data["code"], "busy");
        assert_eq!(busy.data["retryable"], true);
        // The read lane has its own budget, even when the heavy queue is full.
        register(
            &mut pending,
            super::super::dispatch_outcome(
                request(
                    "deadline",
                    "zoom",
                    serde_json::json!({"file":"fixture.rs", "symbol":"fast_zoom", "deadline_ms":0}),
                ),
                &ctx,
                &reads,
            ),
        );
        assert!(pending.cancel_request("queued-255"));
        let ready = pending.poll_ready(&ctx);
        assert_eq!(
            ready
                .iter()
                .find(|r| r.response.id == "queued-255")
                .unwrap()
                .response
                .data["code"],
            "request_cancelled"
        );
        assert_eq!(
            ready
                .iter()
                .find(|r| r.response.id == "deadline")
                .unwrap()
                .response
                .data["code"],
            "deadline_exceeded"
        );
        assert_eq!(reads.heavy.load(Ordering::Acquire), LANE_CAP - 1);
        drop(release);
        finish(&ctx, &mut pending);
        assert_eq!(reads.heavy.load(Ordering::Acquire), 0);
    }

    #[test]
    fn standalone_parallel_tool_wrappers_and_aliases_preserve_response_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("fixture.rs"),
            "fn target() {}\nfn caller() { target(); }\n",
        )
        .unwrap();
        let ctx = fixture(root.path());
        let reads = StandaloneReads::new();
        for command in [
            "grep",
            "glob",
            "outline",
            "zoom",
            "callers",
            "impact",
            "call_tree",
            "trace_to",
            "trace_to_symbol",
            "trace_data",
        ] {
            for wrapped in [false, true] {
                let native = match command {
                    "grep" => serde_json::json!({"path":"fixture.rs", "pattern":"target"}),
                    "glob" => serde_json::json!({"pattern":"**/*.rs"}),
                    "outline" => serde_json::json!({"file":"fixture.rs"}),
                    _ => {
                        serde_json::json!({"file":"fixture.rs", "symbol":"target", "toSymbol":"caller", "expression":"target"})
                    }
                };
                let req = if wrapped {
                    let (name, arguments) = match command {
                        "grep" | "glob" => (command, native.clone()),
                        "outline" => (command, serde_json::json!({"target":"fixture.rs"})),
                        "zoom" => (
                            command,
                            serde_json::json!({"batch":[{"target":"fixture.rs", "symbols":"target"}]}),
                        ),
                        _ => (
                            "callgraph",
                            serde_json::json!({"op":command, "path":"fixture.rs", "symbol":"target", "toSymbol":"caller", "expression":"target"}),
                        ),
                    };
                    request(
                        "parity",
                        "tool_call",
                        serde_json::json!({"name":name, "arguments":arguments}),
                    )
                } else {
                    request("parity", command, native)
                };
                super::super::dispatch(req.clone(), &ctx);
                let mut inline = super::super::dispatch(req.clone(), &ctx);
                assert_ne!(inline.data["code"], "unknown_command", "{inline:?}");
                assert_ne!(inline.data["code"], "invalid_request", "{inline:?}");
                let mut pending = PendingResponses::default();
                register(
                    &mut pending,
                    super::super::dispatch_outcome(req, &ctx, &reads),
                );
                let mut response = finish(&ctx, &mut pending).pop().unwrap();
                // Grep reports measured handler time; only that volatile field
                // is excluded from the inline/deferred response comparison.
                inline.data.as_object_mut().unwrap().remove("search_ms");
                response.data.as_object_mut().unwrap().remove("search_ms");
                assert_eq!(
                    serde_json::to_value(response).unwrap(),
                    serde_json::to_value(inline).unwrap(),
                    "{command}, wrapped={wrapped}"
                );
            }
        }
    }
}
