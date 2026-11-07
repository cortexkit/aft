//! A keyed tool call (one that carries a `call_key`) writes a call-ledger row
//! before it dispatches and settles that row before the transport writer may
//! send its reply: see `LedgerEdge::defer` and `LedgerEdge::proxy`. A reply
//! enters the writer queue when the proxy receives it, so the reply's logged
//! `egress_queue` phase includes the wait for that settlement.
//!
//! The tests below check that neither ledger step waits for other work
//! running on the same project root. The ignored load harness at the end
//! measures `egress_queue` for keyed reads and edits while several sessions
//! read the same root in parallel batches, one session edits it, and
//! background maintenance runs beside them.

use super::*;
use crate::executor::{ExecutorConfig, ExecutorJob};
use crate::run_tool_call::PhaseTrace;
use tool_provider::RouteRole;

const FIRST_CHANNEL: u16 = 42;

/// What the stand-in transport writer saw for one reply: its frame type and
/// the `egress_queue` its trace measured, from proxy entry to writer pickup.
type Written = (FrameType, Option<Duration>);

type Waiters<T> = Arc<StdMutex<HashMap<(RouteChannel, u64), oneshot::Sender<T>>>>;

struct Fixture {
    _dir: tempfile::TempDir,
    root: ProjectRootId,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    routes: HashMap<RouteChannel, RouteIdentity>,
    edge: Arc<tool_provider::LedgerEdge>,
    proxy: WriterSender,
    metrics: Arc<DispatchPathMetrics>,
    admitted: Waiters<DecodedFrame>,
    written: Waiters<Written>,
}

/// A job that occupies the fixture's root until released.
struct Held {
    release: std::sync::mpsc::Sender<()>,
    done: oneshot::Receiver<Response>,
}

impl Held {
    async fn finish(self) {
        let _ = self.release.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.done).await;
    }
}

struct Sample {
    started: Instant,
    admission: Duration,
    egress_queue: Duration,
}

impl Fixture {
    /// One project root with a real call-ledger database, `sessions` tool
    /// provider routes bound to it, and stand-ins for the frame loop (which
    /// receives admitted calls) and the transport writer (which receives
    /// settled replies). Must run inside a tokio runtime.
    fn new(name: &str, sessions: u16) -> Arc<Self> {
        // The module compiles its admission validators before it registers
        // with the daemon; do the same so no measured call pays that one-time
        // cost.
        tool_provider::prepare_admission();
        let (dir, root) = test_support::test_root(name);
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(StdMutex::new(
            crate::db::open(&dir.path().join("aft.db")).expect("open the call-ledger database"),
        )));
        // Sized like the default pool on a ten-core machine: eight general
        // workers and at most four concurrent reads per project root.
        let executor = Arc::new(Executor::with_config(ExecutorConfig {
            pool_size: 8,
            read_cap: 4,
            actor_cap: 4,
            heavy_permits: 2,
            drr_quantum: 1,
        }));
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let routes = (0..sessions)
            .map(|session| {
                let mut identity =
                    test_support::route_identity(&root, &format!("session-{session}"));
                Arc::get_mut(&mut identity.0).unwrap().role = RouteRole::ToolProviderV1;
                (route_key(FIRST_CHANNEL + session, 1), identity)
            })
            .collect();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (edge, mut ready) = tool_provider::LedgerEdge::new();
        let (actual, mut writer) = mpsc::channel::<WriterFrame>(WRITER_QUEUE_CAPACITY);
        let (proxy, _proxy_task) = edge.proxy(actual, Arc::clone(&executor), Arc::clone(&metrics));
        let admitted: Waiters<DecodedFrame> = Arc::default();
        let written: Waiters<Written> = Arc::default();

        let admitted_waiters = Arc::clone(&admitted);
        tokio::spawn(async move {
            while let Some(decoded) = ready.recv().await {
                let header = &decoded.frame.header;
                let key = (route_key(header.channel, header.epoch), header.corr);
                let waiter = admitted_waiters.lock().unwrap().remove(&key);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(decoded);
                }
            }
        });
        let written_waiters = Arc::clone(&written);
        tokio::spawn(async move {
            while let Some(mut frame) = writer.recv().await {
                // The real writer stamps `dequeued` when it takes the frame;
                // the trace turns that into the logged `egress_queue`.
                let dequeued = Instant::now();
                let queued = frame
                    .tool_response_trace
                    .take()
                    .and_then(|trace| {
                        trace.finish(dequeued, dequeued, dequeued, frame.body().len(), None)
                    })
                    .map(|completed| completed.phases.egress_queue);
                let header = &frame.header;
                let key = (route_key(header.channel, header.epoch), header.corr);
                let waiter = written_waiters.lock().unwrap().remove(&key);
                if let Some(waiter) = waiter {
                    let _ = waiter.send((header.ty, queued));
                }
            }
        });

        Arc::new(Self {
            _dir: dir,
            root,
            ctx,
            executor,
            routes,
            edge,
            proxy,
            metrics,
            admitted,
            written,
        })
    }

    fn route(&self, session: u16) -> RouteChannel {
        route_key(FIRST_CHANNEL + session, 1)
    }

    fn call_key(route: RouteChannel, corr: u64) -> String {
        format!("ledger-lane-{}-{corr}", route.channel)
    }

    /// Hand a keyed read to the ledger edge, as the frame loop does when the
    /// request arrives. The receiver resolves once admission has recorded the
    /// call and released it for dispatch.
    fn start_admission(&self, route: RouteChannel, corr: u64) -> oneshot::Receiver<DecodedFrame> {
        let (admitted_tx, admitted_rx) = oneshot::channel();
        self.admitted
            .lock()
            .unwrap()
            .insert((route, corr), admitted_tx);
        let body = json!({
            "name": "read",
            "arguments": {"path": "README.md"},
            "call_key": Self::call_key(route, corr),
        });
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                route.channel,
                route.epoch,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        assert!(
            self.edge
                .defer(
                    decoded,
                    &self.routes,
                    &self.executor,
                    &self.proxy,
                    &self.metrics
                )
                .is_ok(),
            "the ledger edge must take a keyed read"
        );
        admitted_rx
    }

    /// Queue a read's reply on the writer path every tool reply takes. The
    /// receiver resolves when the stand-in transport writer takes the frame.
    async fn send_reply(
        &self,
        route: RouteChannel,
        corr: u64,
        mut trace: PhaseTrace,
    ) -> oneshot::Receiver<Written> {
        let (written_tx, written_rx) = oneshot::channel();
        self.written
            .lock()
            .unwrap()
            .insert((route, corr), written_tx);
        // The tool phases before egress are not under test; mark them so the
        // trace can report its egress phases.
        trace.mark_executor_submitted();
        trace.mark_job_admitted();
        trace.mark_provider_read_done();
        // About the median size of a read reply on a live daemon.
        let body = json!({
            "content": [{"type": "text", "text": "x".repeat(3_800)}],
            "isError": false,
        });
        let frame = Frame::build(
            FrameType::Response,
            control_flags(),
            route.channel,
            route.epoch,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let identity = self.routes.get(&route).expect("route is bound");
        send_traced_tool_response_frame(
            &self.proxy,
            &self.metrics,
            frame,
            ToolResponseWriteTrace::new(
                trace,
                "read".into(),
                crate::logging::ToolCallCaller::Agent,
                identity.project_root.clone(),
                identity.session.clone(),
                route.channel,
                corr,
            ),
        )
        .await
        .expect("the ledger proxy accepts the reply");
        written_rx
    }

    fn ledger_state(&self, route: RouteChannel, corr: u64) -> crate::db::call_ledger::State {
        let identity = self.routes.get(&route).expect("route is bound");
        let key = tool_provider::LedgerEdge::key(identity, &Self::call_key(route, corr));
        let db = self.ctx.db().expect("the fixture opened a database");
        let conn = db.lock().unwrap();
        crate::db::call_ledger::get(&conn, &key)
            .unwrap()
            .expect("admission wrote the call's row")
            .state
    }

    /// Start a job of `lane` on the fixture's root and wait until it runs.
    async fn hold(&self, lane: Lane) -> Held {
        let (started_tx, started_rx) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let job: ExecutorJob = Box::new(move |_| {
            let _ = started_tx.send(());
            let _ = released.recv_timeout(Duration::from_secs(10));
            Response::success("held-root-work", json!({}))
        });
        let done = if lane == Lane::MaintenanceCommit {
            self.executor.submit_maintenance_async(
                self.root.clone(),
                lane,
                "held-root-work".into(),
                job,
            )
        } else {
            self.executor
                .submit_async(self.root.clone(), lane, "held-root-work".into(), job)
        };
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .unwrap_or_else(|_| panic!("the held {lane:?} job never started"))
            .unwrap();
        Held { release, done }
    }

    /// One keyed call end to end: admission, the call's own work on `lane`,
    /// then its reply through the ledger proxy to the writer. The ledger path
    /// does not depend on the tool, so every call is sent as a `read`; `lane`
    /// and `exec` decide what it does to the root while it runs.
    async fn keyed_call(
        self: Arc<Self>,
        route: RouteChannel,
        corr: u64,
        lane: Lane,
        exec: Duration,
    ) -> Sample {
        let started = Instant::now();
        let decoded = self
            .start_admission(route, corr)
            .await
            .expect("admission released the call");
        let admission = started.elapsed();
        let work = self.executor.submit_async(
            self.root.clone(),
            lane,
            format!("call-{}-{corr}", route.channel),
            Box::new(move |_| {
                // Stands in for the call's own work: a read holds a read slot
                // and the root's read gate this long, an edit the write gate.
                std::thread::sleep(exec);
                Response::success("call", json!({}))
            }),
        );
        assert!(work.await.expect("the call completed").success);
        let written = self.send_reply(route, corr, decoded.phase_trace).await;
        let (ty, queued) = written.await.expect("the writer took the reply");
        assert_eq!(
            ty,
            FrameType::Response,
            "the reply settled and kept its body"
        );
        Sample {
            started,
            admission,
            egress_queue: queued.expect("the reply kept its write trace"),
        }
    }
}

/// A keyed reply must reach the writer while `lane` work holds its root. The
/// call is admitted first, so only the reply's settlement can be waiting.
async fn keyed_reply_settles_while_root_is_held(lane: Lane) {
    let fixture = Fixture::new("ledger-settle-beside-root-work", 1);
    let route = fixture.route(0);
    let decoded = tokio::time::timeout(Duration::from_secs(5), fixture.start_admission(route, 1))
        .await
        .expect("admission finished on an idle root")
        .unwrap();
    let held = fixture.hold(lane).await;
    let written = fixture.send_reply(route, 1, decoded.phase_trace).await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), written).await;
    // Release before asserting, so a failure does not leave a worker parked.
    held.finish().await;
    let (ty, _) = outcome
        .unwrap_or_else(|_| panic!("the keyed reply waited for the {lane:?} job holding its root"))
        .unwrap();
    assert_eq!(ty, FrameType::Response);
    assert_eq!(
        fixture.ledger_state(route, 1),
        crate::db::call_ledger::State::Settled
    );
}

/// A keyed call must be admitted while `lane` work holds its root.
async fn keyed_admission_runs_while_root_is_held(lane: Lane) {
    let fixture = Fixture::new("ledger-admit-beside-root-work", 1);
    let route = fixture.route(0);
    let held = fixture.hold(lane).await;
    let admitted =
        tokio::time::timeout(Duration::from_secs(2), fixture.start_admission(route, 1)).await;
    held.finish().await;
    admitted
        .unwrap_or_else(|_| panic!("keyed admission waited for the {lane:?} job holding its root"))
        .unwrap();
    assert_eq!(
        fixture.ledger_state(route, 1),
        crate::db::call_ledger::State::DispatchStarted
    );
}

#[tokio::test]
async fn keyed_reply_settles_while_a_read_runs_on_its_root() {
    keyed_reply_settles_while_root_is_held(Lane::PureRead).await;
}

#[tokio::test]
async fn keyed_reply_settles_while_maintenance_runs_on_its_root() {
    keyed_reply_settles_while_root_is_held(Lane::MaintenanceCommit).await;
}

#[tokio::test]
async fn keyed_reply_settles_while_a_writer_runs_on_its_root() {
    keyed_reply_settles_while_root_is_held(Lane::Mutating).await;
}

#[tokio::test]
async fn keyed_admission_runs_while_a_read_runs_on_its_root() {
    keyed_admission_runs_while_root_is_held(Lane::PureRead).await;
}

#[tokio::test]
async fn keyed_admission_runs_while_a_writer_runs_on_its_root() {
    keyed_admission_runs_while_root_is_held(Lane::Mutating).await;
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

fn summary(label: &str, mut samples: Vec<Duration>) -> String {
    samples.sort();
    let ms = |duration: Duration| duration.as_secs_f64() * 1_000.0;
    format!(
        "{label} n={} p50={:.2}ms p90={:.2}ms max={:.2}ms",
        samples.len(),
        ms(percentile(&samples, 0.5)),
        ms(percentile(&samples, 0.9)),
        ms(*samples.last().unwrap()),
    )
}

/// Six sessions read one root in parallel batches of three to six keyed
/// calls, the way an agent issues several reads, greps and globs in one turn,
/// with a short pause between turns. Each read holds a read slot for its
/// typical live execution time: 1.5 ms for a read, 5 ms for a grep, 9 ms for
/// a glob. Beside them, a seventh session makes a keyed edit that holds the
/// root's write gate for 90 ms (the live median) every 200 ms, and a
/// maintenance job holds the read gate for 8 ms every 40 ms, like a watcher
/// drain after file changes.
///
/// Run with `cargo test -p agent-file-tools --lib -- --ignored --nocapture
/// keyed_read_egress_queue_load_harness`.
#[tokio::test]
#[ignore = "load harness: prints egress_queue percentiles"]
async fn keyed_read_egress_queue_load_harness() {
    const READERS: u16 = 6;
    const BATCHES: u64 = 12;
    let fixture = Fixture::new("ledger-egress-load", READERS + 1);

    let stop = Arc::new(AtomicBool::new(false));
    let maintenance = {
        let fixture = Arc::clone(&fixture);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let drain = fixture.executor.submit_maintenance_async(
                    fixture.root.clone(),
                    Lane::MaintenanceCommit,
                    "watcher-drain-fixture".into(),
                    Box::new(|_| {
                        std::thread::sleep(Duration::from_millis(8));
                        Response::success("watcher-drain-fixture", json!({}))
                    }),
                );
                let _ = drain.await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
    };
    let editor = {
        let fixture = Arc::clone(&fixture);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let route = fixture.route(READERS);
            let mut samples = Vec::new();
            let mut corr = 0u64;
            while !stop.load(Ordering::Relaxed) {
                corr += 1;
                samples.push(
                    Arc::clone(&fixture)
                        .keyed_call(route, corr, Lane::Mutating, Duration::from_millis(90))
                        .await,
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            samples
        })
    };

    let started = Instant::now();
    let mut sessions = tokio::task::JoinSet::new();
    for session in 0..READERS {
        let fixture = Arc::clone(&fixture);
        sessions.spawn(async move {
            let route = fixture.route(session);
            let mut samples = Vec::new();
            let mut corr = 0u64;
            for batch in 0..BATCHES {
                let mut reads = tokio::task::JoinSet::new();
                for _ in 0..3 + (batch + u64::from(session)) % 4 {
                    corr += 1;
                    let exec = match (corr + u64::from(session)) % 10 {
                        0 => Duration::from_millis(9),
                        1 | 2 => Duration::from_millis(5),
                        _ => Duration::from_micros(1_500),
                    };
                    reads.spawn(Arc::clone(&fixture).keyed_call(route, corr, Lane::PureRead, exec));
                }
                while let Some(sample) = reads.join_next().await {
                    samples.push(sample.expect("a keyed read failed"));
                }
                let pause = 20 + (batch * 7 + u64::from(session) * 11) % 40;
                tokio::time::sleep(Duration::from_millis(pause)).await;
            }
            samples
        });
    }
    let mut reads = Vec::new();
    while let Some(session) = sessions.join_next().await {
        reads.extend(session.expect("a reading session failed"));
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let edits = editor.await.expect("the editing session failed");
    let _ = maintenance.await;

    for (label, samples) in [("read", &reads), ("edit", &edits)] {
        eprintln!(
            "{}",
            summary(
                &format!("keyed {label} egress_queue"),
                samples.iter().map(|sample| sample.egress_queue).collect()
            )
        );
        eprintln!(
            "{}",
            summary(
                &format!("keyed {label} admission"),
                samples.iter().map(|sample| sample.admission).collect()
            )
        );
    }
    // Edits are few, so list each one with when it started.
    for (index, edit) in edits.iter().enumerate() {
        eprintln!(
            "keyed edit {index} started at {:.0}ms: admission {:.2}ms, egress_queue {:.2}ms",
            edit.started
                .saturating_duration_since(started)
                .as_secs_f64()
                * 1_000.0,
            edit.admission.as_secs_f64() * 1_000.0,
            edit.egress_queue.as_secs_f64() * 1_000.0,
        );
    }
    eprintln!(
        "load harness wall time {:.0}ms",
        elapsed.as_secs_f64() * 1_000.0
    );
}
