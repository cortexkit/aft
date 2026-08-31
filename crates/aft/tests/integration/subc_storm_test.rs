use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant};

use aft::context::AppContext;
use aft::executor::{Executor, ExecutorConfig, Lane};
use aft::path_identity::ProjectRootId;
use aft::protocol::{ProgressFrame, ProgressKind, PushFrame, RawRequest, Response};
use aft::subc::SubcLifecycleEvent;
use aft::watcher_filter::{
    run_watcher_thread, watcher_dispatch_channel, WatcherDispatchEvent, WatcherFilterConfig,
    WatcherThreadHandle,
};
use serde_json::{json, Value};
use subc_protocol::session::{HealthReport, ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{BindIdentity, Flags, Frame, FrameType, Principal, Priority, RouteTarget};
use subc_transport::{read_frame, write_frame};
use tokio::sync::mpsc;

use super::subc_bridge_test::{self, FakeDaemonInput};

// Latency bounds are production-headroom assertions calibrated for RELEASE
// builds (the daemon ships release; the release-profile storm suite passes
// in ~14s on a loaded box where debug fails the 2s bind bound — same tree,
// A/B established). The main storm test additionally exercises the module's
// REAL 12s bind deadline, which no test-side bound scaling can relax, so it
// is debug-ignored and runs in the gate's release-profile step. The
// remaining storm tests keep meaningful absolute bounds in debug via this
// multiplier: a genuine starvation regression blows past 4x (the pre-fix
// failure ran 60s+, not 2-4s), while unoptimized-build overhead does not.
const DEBUG_BOUND_MULTIPLIER: u32 = if cfg!(debug_assertions) { 4 } else { 1 };
const BIND_ACK_BOUND: Duration = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);
/// Bound for setup binds that are not themselves under test: a cold first bind
/// legitimately pays topology probes, owner claim, and index setup, so setup
/// waits use the production route-bind deadline rather than the tight storm
/// assertion bound.
const SETUP_BIND_BOUND: Duration = Duration::from_secs(12);
const TOOL_BOUND: Duration = Duration::from_secs(5 * DEBUG_BOUND_MULTIPLIER as u64);
const HEALTH_BOUND: Duration = Duration::from_millis(500 * DEBUG_BOUND_MULTIPLIER as u64);
/// Completion pushes ride the bg_events wake nudge, which coalesces at most
/// once per 250ms tick BY DESIGN — so a completion landing just after a tick
/// boundary pays up to a full tick before egress even starts. 700ms left
/// ~zero headroom over that design latency and fired at 709ms on a quiet
/// machine (isolation, 2026-08-09). The bound exists to catch pushes that do
/// not arrive (the lossy-funnel bug class), not to police tick jitter.
const COMPLETION_PUSH_BOUND: Duration = Duration::from_millis(1000 * DEBUG_BOUND_MULTIPLIER as u64);
const ROUTE_BIND_DEADLINE: Duration = Duration::from_secs(12);
const CONCURRENCY_SCENARIO_BOUND: Duration =
    Duration::from_secs(10 * DEBUG_BOUND_MULTIPLIER as u64);
const WATCHER_SINGLE_EVENT_BOUND: Duration =
    Duration::from_secs(10 * DEBUG_BOUND_MULTIPLIER as u64);
const WATCHER_SLICE_READ_BOUND: Duration = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);
const WATCHER_DISPATCH_PATH_CAP: usize = 1_024;
const WATCHER_BACKLOG_EVENTS: usize = 768;

static SYNTHETIC_WATCHER_SENDERS: OnceLock<
    Mutex<Vec<crossbeam_channel::Sender<WatcherDispatchEvent>>>,
> = OnceLock::new();

struct SyntheticRawWatcher(
    #[allow(dead_code)] std::sync::mpsc::Sender<notify::Result<notify::Event>>,
);

#[derive(Clone, Debug)]
struct StormScale {
    roots: usize,
    sessions_per_root: usize,
    storm_for: Duration,
}

#[derive(Clone, Debug)]
struct EgressMeasureConfig {
    sessions: usize,
    responses: usize,
    small_bytes: usize,
    large_bytes: usize,
    read_pause: Duration,
    read_delay: Duration,
    pushes_per_large_response: usize,
}

impl EgressMeasureConfig {
    fn from_env() -> Self {
        Self {
            sessions: env_usize("AFT_EGRESS_SESSIONS", 8).clamp(1, 64),
            responses: env_usize("AFT_EGRESS_RESPONSES", 384).clamp(1, 2_048),
            small_bytes: env_usize("AFT_EGRESS_SMALL_BYTES", 4 * 1024),
            large_bytes: env_usize("AFT_EGRESS_LARGE_BYTES", 256 * 1024),
            read_pause: Duration::from_millis(env_u64("AFT_EGRESS_READ_PAUSE_MS", 500)),
            read_delay: Duration::from_millis(env_u64("AFT_EGRESS_READ_DELAY_MS", 2)),
            pushes_per_large_response: env_usize("AFT_EGRESS_PUSHES_PER_LARGE", 2),
        }
    }
}

#[derive(Debug)]
struct EgressObservation {
    corr: u64,
    requested_bytes: usize,
    frame_bytes: usize,
    latency: Duration,
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

impl StormScale {
    fn from_env() -> Self {
        let mut scale = Self {
            roots: 4,
            sessions_per_root: 2,
            storm_for: Duration::from_secs(6),
        };
        if let Ok(raw) = std::env::var("AFT_STORM_SCALE") {
            if let Some((roots, sessions)) = raw
                .split_once('x')
                .or_else(|| raw.split_once('X'))
                .and_then(|(left, right)| Some((left.parse().ok()?, right.parse().ok()?)))
            {
                scale.roots = roots;
                scale.sessions_per_root = sessions;
            } else if let Ok(roots) = raw.parse::<usize>() {
                scale.roots = roots;
            }
        }
        scale.roots = scale.roots.max(4);
        scale.sessions_per_root = scale.sessions_per_root.max(2);
        scale
    }
}

#[derive(Debug)]
struct RouteSpec {
    channel: u16,
    epoch: u32,
    root_index: usize,
    session: String,
    next_bind_at: Instant,
    next_tool_at: Instant,
    bind_count: usize,
    tool_count: usize,
}

#[derive(Clone, Copy)]
struct StormTiming {
    bind_bound: Duration,
    tool_bound: Duration,
    health_bound: Duration,
    completion_bound: Duration,
    measure_in_storm_stats: bool,
}

impl StormTiming {
    const SETUP: Self = Self {
        bind_bound: SETUP_BIND_BOUND,
        tool_bound: SETUP_BIND_BOUND,
        health_bound: SETUP_BIND_BOUND,
        completion_bound: SETUP_BIND_BOUND,
        measure_in_storm_stats: false,
    };
    const MEASURED: Self = Self {
        bind_bound: BIND_ACK_BOUND,
        tool_bound: TOOL_BOUND,
        health_bound: HEALTH_BOUND,
        completion_bound: COMPLETION_PUSH_BOUND,
        measure_in_storm_stats: true,
    };
}

#[derive(Debug)]
struct BindTiming {
    route: u16,
    started_at: Instant,
    bound: Duration,
    measure_in_storm_stats: bool,
}

#[derive(Debug)]
struct ToolTiming {
    route: u16,
    started_at: Instant,
    name: &'static str,
    bound: Duration,
    measure_in_storm_stats: bool,
}

#[derive(Debug)]
struct HealthTiming {
    started_at: Instant,
    bound: Duration,
    measure_in_storm_stats: bool,
}

#[derive(Debug)]
struct CompletionTiming {
    started_at: Instant,
    bound: Duration,
    measure_in_storm_stats: bool,
}

#[derive(Default, Debug)]
struct StormStats {
    bind_latencies: Vec<Duration>,
    max_tool_latency: Duration,
    max_health_latency: Duration,
    max_completion_latency: Duration,
}

struct SlowEmbeddingServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SlowEmbeddingServer {
    fn start(delay: Duration) -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind embedding mock");
        let addr = listener.local_addr().expect("embedding mock addr");
        listener
            .set_nonblocking(true)
            .expect("embedding mock nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread::spawn(move || handle_embedding_request(stream, delay));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) => panic!("embedding mock accept failed: {error}"),
                }
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for SlowEmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(handle) = self.handle.take() {
            handle.join().expect("embedding mock joins");
        }
    }
}

fn handle_embedding_request(mut stream: std::net::TcpStream, delay: Duration) {
    stream
        .set_nonblocking(false)
        .expect("embedding request stream blocking mode");
    let mut reader = BufReader::new(stream.try_clone().expect("clone embedding stream"));
    let mut headers = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read embedding header");
        if read == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        headers.push_str(&line);
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("read embedding body");
    let request_body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let input_count = request_body
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::len)
        .or_else(|| request_body.get("input").and_then(Value::as_str).map(|_| 1))
        .unwrap_or(1)
        .max(1);
    thread::sleep(delay);
    let data: Vec<Value> = (0..input_count)
        .map(|index| {
            json!({
                "embedding": [index as f64 + 0.1, index as f64 + 0.2, index as f64 + 0.3],
                "index": index,
            })
        })
        .collect();
    let response_body = json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(), response_body
    );
    stream
        .write_all(response.as_bytes())
        .expect("write embedding response");
}

fn storm_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    match req.command.as_str() {
        "configure" => {
            if let Some(delay) = configure_sleep_delay(&req) {
                thread::sleep(delay);
            }
            aft::commands::configure::handle_configure(&req, ctx)
        }
        "subc_storm_inject_dispatch" => inject_dispatch_events(&req, ctx),
        "subc_storm_inject_raw_paths" => inject_raw_watcher_paths(&req, ctx),
        "subc_storm_watcher_pending" => watcher_pending_response(&req, ctx),
        "subc_storm_egress_payload" => egress_payload_response(&req, ctx),
        _ => subc_bridge_test::bridge_dispatch(req, ctx),
    }
}

fn egress_payload_response(req: &RawRequest, ctx: &AppContext) -> Response {
    let payload_bytes = req
        .params
        .get("payload_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let push_count = req
        .params
        .get("push_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if let Some(sender) = ctx.progress_sender_handle() {
        for index in 0..push_count {
            sender(PushFrame::Progress(ProgressFrame {
                frame_type: "progress",
                request_id: format!("{}-push-{index}", req.id),
                kind: ProgressKind::Stdout,
                chunk: "p".repeat(256),
            }));
        }
    }
    Response::success(
        &req.id,
        json!({
            "payload": "x".repeat(payload_bytes),
            "payload_bytes": payload_bytes,
        }),
    )
}

fn inject_dispatch_events(req: &RawRequest, ctx: &AppContext) -> Response {
    let Some(root) = ctx.config().project_root.clone() else {
        return Response::error(
            &req.id,
            "missing_project_root",
            "storm root is not configured",
        );
    };
    let Some(events) = req.params.get("events").and_then(Value::as_array) else {
        return Response::error(&req.id, "invalid_request", "events must be an array");
    };
    let (tx, rx) = watcher_dispatch_channel();
    let mut path_count = 0usize;
    for event in events {
        let dispatch = if event.get("rescan").and_then(Value::as_bool) == Some(true) {
            WatcherDispatchEvent::RescanRequired
        } else {
            let Some(paths) = event.get("paths").and_then(Value::as_array) else {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "each event needs paths or rescan",
                );
            };
            let paths = paths
                .iter()
                .filter_map(Value::as_str)
                .map(|path| root.join(path))
                .collect::<Vec<_>>();
            path_count += paths.len();
            WatcherDispatchEvent::Paths(paths)
        };
        tx.send(dispatch).expect("inject synthetic watcher event");
    }
    ctx.stop_watcher_runtime();
    *ctx.watcher_rx().lock() = Some(rx);
    SYNTHETIC_WATCHER_SENDERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("synthetic watcher sender lock")
        .push(tx);
    Response::success(
        &req.id,
        json!({ "events": events.len(), "paths": path_count }),
    )
}

fn inject_raw_watcher_paths(req: &RawRequest, ctx: &AppContext) -> Response {
    let Some(root) = ctx.config().project_root.clone() else {
        return Response::error(
            &req.id,
            "missing_project_root",
            "storm root is not configured",
        );
    };
    let Some(paths) = req.params.get("paths").and_then(Value::as_array) else {
        return Response::error(&req.id, "invalid_request", "paths must be an array");
    };
    let paths = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    let path_count = paths.len();
    let matcher = ctx.shared_gitignore();
    let matcher_generation = ctx.gitignore_generation();
    let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let filter_root = std::fs::canonicalize(&root).unwrap_or(root);
    let config = WatcherFilterConfig::new(filter_root, None);
    let handle = thread::spawn(move || {
        run_watcher_thread(
            config,
            Vec::new(),
            matcher,
            matcher_generation,
            dispatch_tx,
            thread_shutdown,
            move |_, _, raw_tx| {
                let mut event =
                    notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any));
                event.paths = paths;
                raw_tx.send(Ok(event)).map_err(|error| error.to_string())?;
                Ok::<SyntheticRawWatcher, String>(SyntheticRawWatcher(raw_tx))
            },
        );
    });
    ctx.stop_watcher_runtime();
    ctx.install_watcher_runtime(dispatch_rx, WatcherThreadHandle::new(shutdown, handle));
    Response::success(&req.id, json!({ "raw_paths": path_count }))
}

fn watcher_pending_response(req: &RawRequest, ctx: &AppContext) -> Response {
    let pending = ctx
        .watcher_rx()
        .lock()
        .as_ref()
        .map_or(0, crossbeam_channel::Receiver::len)
        + ctx.watcher_drain_pending_path_count();
    Response::success(
        &req.id,
        json!({
            "pending": pending,
            "slices": ctx.watcher_drain_path_slice_count(),
        }),
    )
}

fn configure_sleep_delay(req: &RawRequest) -> Option<Duration> {
    let tiers = req.params.get("config")?.as_array()?;
    for tier in tiers {
        let Some(doc) = tier.get("doc").and_then(Value::as_str) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(doc) else {
            continue;
        };
        if let Some(delay_ms) = value
            .get("subc_test_configure_sleep_ms")
            .and_then(Value::as_u64)
        {
            return Some(Duration::from_millis(delay_ms));
        }
    }
    None
}

#[test]
fn max_paths_single_event() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "max_paths_single_event",
        // Windows CI runs the integration binary up to 5x slower under
        // contention (memory 6987); the watchdog is a hang backstop, not a
        // performance bound, so keep it generous.
        Duration::from_secs(120),
        drive_max_paths_single_event,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
#[ignore = "quiet-box latency harness; run explicitly with --ignored --nocapture"]
fn max_paths_single_event_benchmark() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "max_paths_single_event_benchmark",
        Duration::from_secs(30),
        drive_max_paths_single_event_with_report,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn reads_during_maintenance() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "reads_during_maintenance",
        Duration::from_secs(30),
        drive_reads_during_maintenance,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn pool_size_two() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch_and_executor_config(
        "pool_size_two",
        Duration::from_secs(30),
        drive_pool_size_two,
        |_, _, _| {},
        storm_dispatch,
        ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
            ..ExecutorConfig::default()
        },
    );
}

#[test]
fn generation_supersession_mid_drain() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "generation_supersession_mid_drain",
        // Readiness and watcher drain have their own observable deadlines. This outer
        // watchdog only detects a hang and must outlive both under suite contention.
        Duration::from_secs(180),
        drive_generation_supersession_mid_drain,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn ignore_edit_ordering() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "ignore_edit_ordering",
        Duration::from_secs(30),
        drive_ignore_edit_ordering,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn rename_pair() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "rename_pair",
        Duration::from_secs(30),
        drive_rename_pair,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn overflow_control() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "overflow_control",
        Duration::from_secs(30),
        drive_overflow_control,
        |_, _, _| {},
        storm_dispatch,
    );
}

async fn drive_max_paths_single_event(input: FakeDaemonInput) {
    drive_max_paths_single_event_inner(input, false).await;
}

async fn drive_max_paths_single_event_with_report(input: FakeDaemonInput) {
    drive_max_paths_single_event_inner(input, true).await;
}

async fn drive_max_paths_single_event_inner(input: FakeDaemonInput, report: bool) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    prepare_storm_root(&root);
    let paths = (0..WATCHER_DISPATCH_PATH_CAP)
        .map(|index| format!("src/max_path_{index:04}.rs"))
        .collect::<Vec<_>>();
    for (index, path) in paths.iter().enumerate() {
        write_storm_file(&root.join(path), &format!("max_old_{index:04}"));
    }
    init_git(&root);

    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 30_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "max-paths").await;
    for (index, path) in paths.iter().enumerate() {
        write_storm_file(&root.join(path), &format!("max_new_{index:04}"));
    }

    corr += 1;
    let started = Instant::now();
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_dispatch",
        json!({ "events": [{ "paths": paths }] }),
    );
    let injected = expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    assert_eq!(injected.get("events").and_then(Value::as_u64), Some(1));
    assert_eq!(
        injected.get("paths").and_then(Value::as_u64),
        Some(WATCHER_DISPATCH_PATH_CAP as u64)
    );

    corr += 1;
    let read_started = Instant::now();
    send_tool(
        &tx,
        1,
        corr,
        "read",
        json!({ "filePath": "README.md", "limit": 20 }),
    );
    expect_tool_response(&mut rx, corr, WATCHER_SLICE_READ_BOUND).await;
    let read_elapsed = read_started.elapsed();
    assert!(
        read_elapsed <= WATCHER_SLICE_READ_BOUND,
        "read queued during watcher drain took {read_elapsed:?}"
    );

    wait_for_watcher_empty(&tx, &mut rx, &mut corr).await;
    let slices_after = watcher_progress(&tx, &mut rx, &mut corr)
        .await
        .get("slices")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let slice_count = slices_after;
    assert_eq!(
        slice_count, 1,
        "1,024 paths must fit one 2,048-path watcher slice"
    );
    for marker in ["max_new_0000", "max_new_0512", "max_new_1023"] {
        wait_for_grep_total(&tx, &mut rx, &mut corr, marker, 1).await;
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed <= WATCHER_SINGLE_EVENT_BOUND,
        "1,024-path watcher event took {elapsed:?}; mid-drain read took {read_elapsed:?}"
    );
    if report {
        eprintln!(
            "max_paths_single_event: slices={slice_count} total={elapsed:?} mid_drain_read={read_elapsed:?}"
        );
    }
    send_goodbye_and_wait(&tx).await;
}

async fn drive_reads_during_maintenance(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    prepare_storm_root(&root);
    init_git(&root);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 31_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "reads-maintenance").await;

    let mut events = Vec::with_capacity(WATCHER_BACKLOG_EVENTS);
    for index in 0..WATCHER_BACKLOG_EVENTS {
        let path = format!("src/maintenance_{index:04}.rs");
        write_storm_file(&root.join(&path), &format!("maintenance_marker_{index:04}"));
        events.push(json!({ "paths": [path] }));
    }
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_dispatch",
        json!({ "events": events }),
    );
    expect_tool_response(&mut rx, corr, TOOL_BOUND).await;

    let started = Instant::now();
    let mut expected = HashSet::new();
    for index in 0..6 {
        corr += 1;
        expected.insert(corr);
        send_tool(
            &tx,
            1,
            corr,
            "read",
            json!({ "filePath": "README.md", "limit": 20 }),
        );
        corr += 1;
        expected.insert(corr);
        send_tool(
            &tx,
            1,
            corr,
            "grep",
            json!({ "pattern": format!("maintenance_marker_{:04}", index * 100) }),
        );
    }
    collect_tool_responses(&mut rx, expected, CONCURRENCY_SCENARIO_BOUND).await;
    let elapsed = started.elapsed();
    // I-A target: Once interactive tool-lane isolation exists, enforce its tighter latency budget.
    assert!(
        elapsed <= CONCURRENCY_SCENARIO_BOUND,
        "concurrent reads took {elapsed:?}"
    );
    wait_for_watcher_empty(&tx, &mut rx, &mut corr).await;
    send_goodbye_and_wait(&tx).await;
}

async fn drive_pool_size_two(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    assert_eq!(session.executor.pool_size(), 2, "storm executor pool seam");
    let root = session.root1.clone();
    prepare_storm_root(&root);
    init_git(&root);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 32_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "pool-two").await;

    let path = "src/pool_two.rs";
    write_storm_file(&root.join(path), "pool_two_maintenance_marker");
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_dispatch",
        json!({
            "events": (0..WATCHER_BACKLOG_EVENTS)
                .map(|_| json!({ "paths": [path] }))
                .collect::<Vec<_>>()
        }),
    );
    expect_tool_response(&mut rx, corr, TOOL_BOUND).await;

    corr += 1;
    let read_corr = corr;
    send_tool(
        &tx,
        1,
        read_corr,
        "read",
        json!({ "filePath": "README.md", "limit": 20 }),
    );
    corr += 1;
    let grep_corr = corr;
    send_tool(
        &tx,
        1,
        grep_corr,
        "grep",
        json!({ "pattern": "pool_two_maintenance_marker" }),
    );
    collect_tool_responses(
        &mut rx,
        HashSet::from([read_corr, grep_corr]),
        CONCURRENCY_SCENARIO_BOUND,
    )
    .await;
    wait_for_watcher_empty(&tx, &mut rx, &mut corr).await;
    send_goodbye_and_wait(&tx).await;
}

async fn drive_generation_supersession_mid_drain(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    prepare_storm_root(&root);
    init_git(&root);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 33_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "generation-old").await;

    let mut events = Vec::with_capacity(WATCHER_BACKLOG_EVENTS);
    for index in 0..WATCHER_BACKLOG_EVENTS {
        let path = format!("src/superseded_{index:04}.rs");
        events.push(json!({ "paths": [path] }));
    }
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_dispatch",
        json!({ "events": events }),
    );
    let injected = expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    assert_eq!(
        injected.get("events").and_then(Value::as_u64),
        Some(WATCHER_BACKLOG_EVENTS as u64)
    );

    corr += 1;
    send_bind(
        &tx,
        2,
        corr,
        &root,
        "generation-new",
        storm_project_config(true, false, true, 0),
    );
    expect_ack_within(&mut rx, corr, SETUP_BIND_BOUND).await;
    wait_for_ready_health(
        &tx,
        &mut rx,
        &mut corr,
        std::slice::from_ref(&root),
        &HashSet::new(),
    )
    .await;
    // The generation handoff is complete when the synthetic watcher backlog reports
    // empty; polling that state avoids assuming 768 queued events drain within 2s.
    wait_for_watcher_empty(&tx, &mut rx, &mut corr).await;

    let stale_tail_path = "src/superseded_0767.rs";
    write_storm_file(&root.join(stale_tail_path), "stale_tail_marker");
    tokio::time::sleep(Duration::from_millis(250)).await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "stale_tail_marker", 0).await;

    let current_path = "src/current_generation.rs";
    write_storm_file(&root.join(current_path), "current_generation_marker");
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_dispatch",
        json!({ "events": [{ "paths": [current_path] }] }),
    );
    expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    wait_for_watcher_empty(&tx, &mut rx, &mut corr).await;
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "read",
        json!({ "filePath": current_path, "limit": 20 }),
    );
    let current = expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    assert!(
        current.to_string().contains("current_generation_marker"),
        "new generation read returned inconsistent state: {current:?}"
    );
    send_goodbye_and_wait(&tx).await;
}

async fn drive_ignore_edit_ordering(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    prepare_storm_root(&root);
    write_storm_file(&root.join("flip_unignored.rs"), "newly_unignored_marker");
    write_storm_file(&root.join("flip_ignored.rs"), "newly_ignored_marker");
    std::fs::write(root.join(".gitignore"), "flip_unignored.rs\n").expect("initial ignore");
    init_git(&root);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 34_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "ignore-ordering").await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "newly_unignored_marker", 0).await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "newly_ignored_marker", 1).await;

    std::fs::write(root.join(".gitignore"), "flip_ignored.rs\n").expect("flipped ignore");
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_raw_paths",
        json!({ "paths": [".gitignore", "flip_unignored.rs", "flip_ignored.rs"] }),
    );
    expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "newly_unignored_marker", 1).await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "newly_ignored_marker", 0).await;
    send_goodbye_and_wait(&tx).await;
}

async fn drive_rename_pair(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    prepare_storm_root(&root);
    write_storm_file(&root.join("tracked_rename.rs"), "rename_source_marker");
    std::fs::write(root.join(".gitignore"), "target/\n").expect("target ignore");
    init_git(&root);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 35_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "rename-pair").await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "rename_source_marker", 1).await;

    std::fs::create_dir_all(root.join("target")).expect("ignored target dir");
    std::fs::rename(
        root.join("tracked_rename.rs"),
        root.join("target/renamed.rs"),
    )
    .expect("rename into ignored directory");
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_raw_paths",
        json!({ "paths": ["tracked_rename.rs", "target/renamed.rs"] }),
    );
    expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "rename_source_marker", 0).await;
    send_goodbye_and_wait(&tx).await;
}

async fn drive_overflow_control(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    prepare_storm_root(&root);
    init_git(&root);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 36_000_u64;
    bind_ready_search_root(&tx, &mut rx, &mut corr, &root, "overflow-control").await;

    write_storm_file(&root.join("granular_before.rs"), "granular_before_marker");
    write_storm_file(&root.join("overflow_only.rs"), "overflow_rescan_marker");
    write_storm_file(&root.join("granular_after.rs"), "granular_after_marker");
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_storm_inject_dispatch",
        json!({
            "events": [
                { "paths": ["granular_before.rs"] },
                { "rescan": true },
                { "paths": ["granular_after.rs"] }
            ]
        }),
    );
    expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    wait_for_grep_total(&tx, &mut rx, &mut corr, "overflow_rescan_marker", 1).await;
    send_goodbye_and_wait(&tx).await;
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "asserts production-calibrated absolute latencies (2s bind headroom, the module's real 12s bind deadline); a debug build under load cannot honor them even when correct — run via the gate's release-storm step"
)]
fn subc_storm_rebinds_stay_live_under_build_and_tool_traffic() {
    let scale = StormScale::from_env();
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "subc_storm_rebinds_stay_live_under_build_and_tool_traffic",
        Duration::from_secs(90),
        move |input| drive_storm_daemon(input, scale),
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn fresh_worktree_bind_does_not_starve_parent_reads_or_acquire_parent_writers() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "fresh_worktree_bind_does_not_starve_parent_reads_or_acquire_parent_writers",
        // Root readiness is an observable poll with a 120-second budget. The outer
        // watchdog is only a hang backstop; the parent-read phase keeps its tight bound.
        Duration::from_secs(180),
        drive_fresh_worktree_borrow_only_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn subc_storm_heavy_init_saturation_does_not_delay_fresh_bind() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch_and_executor_config(
        "subc_storm_heavy_init_saturation_does_not_delay_fresh_bind",
        Duration::from_secs(30),
        drive_heavy_init_saturation_daemon,
        |_, _, _| {},
        storm_dispatch,
        ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 2,
            drr_quantum: 1,
            ..ExecutorConfig::default()
        },
    );
}

#[test]
fn subc_storm_many_standing_roots_yield_before_cold_admission_and_reads_finish_in_budget() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch_and_executor_config(
        "subc_storm_many_standing_roots_yield_before_cold_admission",
        Duration::from_secs(45),
        drive_standing_yield_daemon,
        |_, _, _| {},
        storm_dispatch,
        ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 2,
            drr_quantum: 1,
            ..ExecutorConfig::default()
        },
    );
}

#[test]
fn subc_storm_completion_channel_saturation_does_not_delay_binds() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "subc_storm_completion_channel_saturation_does_not_delay_binds",
        Duration::from_secs(30),
        drive_completion_saturation_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn subc_storm_slow_config_read_does_not_delay_bind_ack() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "subc_storm_slow_config_read_does_not_delay_bind_ack",
        Duration::from_secs(30),
        drive_slow_config_read_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn subc_storm_detach_rebind_replays_completion_and_preserves_bash_task() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch_and_lifecycle_probe(
        "subc_storm_detach_rebind_replays_completion_and_preserves_bash_task",
        Duration::from_secs(45),
        drive_detach_rebind_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn subc_storm_route_bind_deadline_errors_and_retry_recovers() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "subc_storm_route_bind_deadline_errors_and_retry_recovers",
        Duration::from_secs(40),
        drive_route_bind_deadline_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
#[ignore = "manual M1 measurement rig; emits one raw row per response"]
fn subc_egress_hol_measurement() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .is_test(true)
        .try_init();
    let config = EgressMeasureConfig::from_env();
    eprintln!("EGRESS_CONFIG {config:?}");
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "subc_egress_hol_measurement",
        Duration::from_secs(180),
        move |input| drive_egress_measurement_daemon(input, config),
        |_, _, _| {},
        storm_dispatch,
    );
}

#[test]
fn subc_storm_same_or_lower_epoch_rebind_is_rejected() {
    subc_bridge_test::run_subc_bridge_test_with_dispatch(
        "subc_storm_same_or_lower_epoch_rebind_is_rejected",
        Duration::from_secs(30),
        drive_epoch_rebind_rejection_daemon,
        |_, _, _| {},
        storm_dispatch,
    );
}

async fn drive_egress_measurement_daemon(input: FakeDaemonInput, config: EgressMeasureConfig) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let mut stream = session.stream;
    let root = session.root1;

    for channel in 1..=config.sessions as u16 {
        write_measure_bind(
            &mut stream,
            channel,
            &root,
            &format!("egress-session-{channel}"),
        )
        .await;
    }

    let mut pending = HashMap::new();
    let corr_base = 800_000_u64;
    for index in 0..config.responses {
        let corr = corr_base + index as u64;
        let channel = (index % config.sessions + 1) as u16;
        let large = index % 4 == 0;
        let payload_bytes = if large {
            config.large_bytes
        } else {
            config.small_bytes
        };
        let push_count = if large {
            config.pushes_per_large_response
        } else {
            0
        };
        let frame = build_measure_tool_frame(channel, corr, payload_bytes, push_count);
        write_frame(&mut stream, &frame)
            .await
            .expect("write egress measurement request");
        pending.insert(corr, (payload_bytes, Instant::now()));
    }

    tokio::time::sleep(config.read_pause).await;
    let mut observations = Vec::with_capacity(config.responses);
    let mut push_frames = 0usize;
    let deadline = Instant::now() + Duration::from_secs(150);
    while !pending.is_empty() && Instant::now() < deadline {
        let Some(frame) = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stream))
            .await
            .expect("egress measurement read timeout")
            .expect("egress measurement frame read")
        else {
            break;
        };
        match frame.header.ty {
            FrameType::Response if frame.header.channel != 0 => {
                if let Some((requested_bytes, started_at)) = pending.remove(&frame.header.corr) {
                    let observation = EgressObservation {
                        corr: frame.header.corr,
                        requested_bytes,
                        frame_bytes: frame.body.len() + frame.header.encode().len(),
                        latency: started_at.elapsed(),
                    };
                    eprintln!(
                        "EGRESS_OBS corr={} requested_bytes={} frame_bytes={} latency_ms={:.3}",
                        observation.corr,
                        observation.requested_bytes,
                        observation.frame_bytes,
                        observation.latency.as_secs_f64() * 1_000.0,
                    );
                    observations.push(observation);
                }
            }
            FrameType::Push => push_frames += 1,
            _ => {}
        }
        if !config.read_delay.is_zero() {
            tokio::time::sleep(config.read_delay).await;
        }
    }

    assert!(
        pending.is_empty(),
        "measurement responses did not drain: {}",
        pending.len()
    );
    print_egress_summary(&observations, push_frames);
    drop(stream);
}

async fn drive_fresh_worktree_borrow_only_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let (tx, mut rx) = start_io(session.stream);
    let (fixture, parent_root, worktree_root) = create_parent_and_linked_worktree();
    let storage_dir = fixture.path().join("artifact-storage");
    let mut corr = 20_000_u64;

    send_bind(
        &tx,
        1,
        corr,
        &parent_root,
        "storm-parent",
        artifact_storm_config(&storage_dir),
    );
    // Setup bind: this test asserts the WORKTREE bind's behavior below, not
    // cold parent-bind latency. A cold first bind legitimately pays topology
    // probes, owner claim, and index setup, so give it the production
    // route-bind deadline instead of the tight storm assertion bound.
    expect_ack_within(&mut rx, corr, SETUP_BIND_BOUND).await;

    let roots = vec![parent_root.clone()];
    wait_for_ready_health(&tx, &mut rx, &mut corr, &roots, &HashSet::new()).await;
    let shared_key = aft::search_index::artifact_cache_key(&parent_root);
    assert_eq!(
        shared_key,
        aft::search_index::artifact_cache_key(&worktree_root),
        "linked worktree must borrow the parent's artifact key"
    );
    aft::root_cache::enable_writer_lease_acquisition_counts_for_test();

    corr += 1;
    let bind_corr = corr;
    let started = Instant::now();
    send_bind(
        &tx,
        2,
        bind_corr,
        &worktree_root,
        "storm-worktree",
        artifact_storm_config(&storage_dir),
    );
    corr += 1;
    let read_corr = corr;
    send_tool(
        &tx,
        1,
        read_corr,
        "read",
        json!({ "filePath": "README.md", "limit": 20 }),
    );
    corr += 1;
    let grep_corr = corr;
    send_tool(
        &tx,
        1,
        grep_corr,
        "grep",
        json!({ "pattern": "root_alpha", "path": "src" }),
    );

    let mut bind_done = false;
    let mut read_done = false;
    let mut grep_done = false;
    let parent_read_bound = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);
    let deadline = Instant::now() + parent_read_bound;
    while !(bind_done && read_done && grep_done) {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        assert!(
            !remaining.is_zero(),
            "fresh worktree bind starved parent reads for more than {parent_read_bound:?}"
        );
        let frame = read_frame_from_rx(&mut rx, remaining, "worktree bind and parent reads").await;
        if is_ack(&frame, bind_corr) {
            bind_done = true;
            continue;
        }
        if frame.header.ty == FrameType::Error
            && (frame.header.corr == read_corr || frame.header.corr == grep_corr)
        {
            let body: Value = serde_json::from_slice(&frame.body).expect("tool error body");
            panic!("parent read failed during worktree bind: {body:?}");
        }
        if frame.header.ty == FrameType::Response && frame.header.corr == read_corr {
            read_done = true;
        }
        if frame.header.ty == FrameType::Response && frame.header.corr == grep_corr {
            grep_done = true;
        }
    }
    assert!(
        started.elapsed() <= parent_read_bound,
        "fresh worktree bind and parent reads took {:?}, exceeding {:?}",
        started.elapsed(),
        parent_read_bound
    );

    let worktree_writer_acquisitions = || {
        let callgraph = aft::root_cache::writer_lease_acquisition_count_for_test(
            aft::root_cache::RootCacheDomain::Callgraph,
            &shared_key,
            &worktree_root,
        );
        let inspect = aft::root_cache::writer_lease_acquisition_count_for_test(
            aft::root_cache::RootCacheDomain::Inspect,
            &shared_key,
            &worktree_root,
        );
        (callgraph, inspect)
    };
    let (callgraph_acquisitions, inspect_acquisitions) = worktree_writer_acquisitions();
    assert_eq!(
        callgraph_acquisitions + inspect_acquisitions,
        0,
        "linked worktree session acquired writer capability on shared key {shared_key}: callgraph={callgraph_acquisitions}, inspect={inspect_acquisitions}"
    );

    let callgraph_dir = storage_dir.join("callgraph").join(&shared_key);
    let writer_shaped = aft::callgraph_store::CallGraphStore::open_ready_no_rebuild(
        callgraph_dir.clone(),
        worktree_root.clone(),
    )
    .expect("worktree writer opener should fail closed");
    assert!(
        writer_shaped.is_none(),
        "borrow-only roots must not receive a writable-shaped callgraph store"
    );
    let _reader =
        aft::callgraph_store::CallGraphStore::open_readonly(callgraph_dir, worktree_root.clone())
            .expect("worktree read-only opener should remain available");
    let (callgraph_acquisitions, inspect_acquisitions) = worktree_writer_acquisitions();
    assert_eq!(
        callgraph_acquisitions + inspect_acquisitions,
        0,
        "borrow-only artifact API handed out writer capability on shared key {shared_key}: callgraph={callgraph_acquisitions}, inspect={inspect_acquisitions}"
    );
    send_goodbye_and_wait(&tx).await;
}

async fn drive_storm_daemon(input: FakeDaemonInput, scale: StormScale) {
    let delay_ms = std::env::var("AFT_STORM_EMBED_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(5_000);
    let embedding = SlowEmbeddingServer::start(Duration::from_millis(delay_ms));
    write_user_semantic_config(&input.user_config_path, &embedding.base_url);
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let (tx, mut rx) = start_io(session.stream);
    let (_tempdirs, roots) = create_storm_roots(scale.roots);
    let semantic_roots: HashSet<usize> = (0..roots.len()).filter(|index| index % 2 == 0).collect();

    aft::commands::configure::reset_semantic_stale_generation_discards_for_test();
    aft::commands::configure::reset_configure_artifact_load_attempts_for_test();
    let mut routes = build_route_specs(&scale, &roots);
    let mut corr = 10_000_u64;
    let mut pending_binds = HashMap::new();
    let mut pending_tools = HashMap::new();
    let mut pending_health = HashMap::new();
    let mut pending_completions = HashMap::new();

    for route in &routes {
        corr += 1;
        send_bind_epoch(
            &tx,
            route.channel,
            route.epoch,
            corr,
            &roots[route.root_index],
            &route.session,
            storm_project_config(true, semantic_roots.contains(&route.root_index), false, 0),
        );
        pending_binds.insert(
            corr,
            BindTiming {
                route: route.channel,
                started_at: Instant::now(),
                bound: StormTiming::SETUP.bind_bound,
                measure_in_storm_stats: false,
            },
        );
    }
    let mut setup_stats = StormStats::default();
    drain_until_idle(
        &mut rx,
        &mut pending_binds,
        &mut pending_tools,
        &mut pending_health,
        &mut pending_completions,
        &mut setup_stats,
        Duration::from_secs(20),
    )
    .await;

    // A freshly linked harness can page in executor and transport code while the
    // first roots begin their background builds. Exercise the same per-route
    // rebind, tool, completion, and watcher traffic under the module's real
    // setup deadline before the measured storm. The warm-up is a readiness
    // barrier, not a relaxed latency assertion: the following phase retains the
    // calibrated production bounds and starts only after every warm-up frame drains.
    run_storm_phase(
        &tx,
        &mut rx,
        &mut routes,
        &roots,
        &semantic_roots,
        &mut corr,
        &mut pending_binds,
        &mut pending_tools,
        &mut pending_health,
        &mut pending_completions,
        &mut setup_stats,
        Duration::from_secs(2),
        StormTiming::SETUP,
    )
    .await;
    drain_until_idle(
        &mut rx,
        &mut pending_binds,
        &mut pending_tools,
        &mut pending_health,
        &mut pending_completions,
        &mut setup_stats,
        Duration::from_secs(20),
    )
    .await;

    let mut stats = StormStats::default();
    run_storm_phase(
        &tx,
        &mut rx,
        &mut routes,
        &roots,
        &semantic_roots,
        &mut corr,
        &mut pending_binds,
        &mut pending_tools,
        &mut pending_health,
        &mut pending_completions,
        &mut stats,
        scale.storm_for,
        StormTiming::MEASURED,
    )
    .await;
    drain_until_idle(
        &mut rx,
        &mut pending_binds,
        &mut pending_tools,
        &mut pending_health,
        &mut pending_completions,
        &mut stats,
        Duration::from_secs(20),
    )
    .await;
    wait_for_ready_health(&tx, &mut rx, &mut corr, &roots, &semantic_roots).await;
    assert_eq!(
        aft::commands::configure::semantic_stale_generation_discards_for_test(),
        0,
        "equivalent rebinds must not discard completed semantic builds as stale"
    );
    assert!(
        aft::commands::configure::configure_artifact_load_attempts_for_test() >= roots.len(),
        "fleet storm did not exercise one post-ack artifact load per cold-ish root"
    );
    assert_bind_stats(&stats.bind_latencies);
    assert!(
        stats.max_tool_latency <= TOOL_BOUND,
        "max tool latency {:?} exceeded {:?}",
        stats.max_tool_latency,
        TOOL_BOUND
    );
    assert!(
        stats.max_health_latency <= HEALTH_BOUND,
        "max health latency {:?} exceeded {:?}",
        stats.max_health_latency,
        HEALTH_BOUND
    );
    assert!(
        stats.max_completion_latency <= COMPLETION_PUSH_BOUND,
        "max reliable completion push latency {:?} exceeded {:?}",
        stats.max_completion_latency,
        COMPLETION_PUSH_BOUND
    );
    send_goodbye_and_wait(&tx).await;
}

async fn run_storm_phase(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    routes: &mut [RouteSpec],
    roots: &[PathBuf],
    semantic_roots: &HashSet<usize>,
    corr: &mut u64,
    pending_binds: &mut HashMap<u64, BindTiming>,
    pending_tools: &mut HashMap<u64, ToolTiming>,
    pending_health: &mut HashMap<u64, HealthTiming>,
    pending_completions: &mut HashMap<String, CompletionTiming>,
    stats: &mut StormStats,
    run_for: Duration,
    timing: StormTiming,
) {
    // Initial RouteBindAck messages establish route readiness. Restart the
    // schedule after that barrier so cold setup cannot collapse the first
    // rebinds and tool calls into a catch-up burst.
    let started_at = Instant::now();
    for route in &mut *routes {
        route.next_bind_at =
            started_at + Duration::from_millis(1_300 + u64::from(route.channel % 5) * 80);
        route.next_tool_at =
            started_at + Duration::from_millis(200 + u64::from(route.channel % 3) * 75);
    }

    let deadline = started_at + run_for;
    let mut next_health_at = started_at;
    while Instant::now() < deadline {
        let now = Instant::now();
        for route in &mut *routes {
            let route_has_pending_bind = pending_binds
                .values()
                .any(|bind| bind.route == route.channel);
            if now >= route.next_bind_at && !route_has_pending_bind {
                route.bind_count += 1;
                route.epoch = route
                    .epoch
                    .checked_add(1)
                    .expect("storm route epoch should not overflow");
                *corr += 1;
                let semantic_enabled = semantic_roots.contains(&route.root_index);
                let config_change = route.bind_count % 10 == 0 && !semantic_enabled;
                if !semantic_enabled {
                    touch_between_rebinds(&roots[route.root_index], route.bind_count);
                }
                send_bind_epoch(
                    tx,
                    route.channel,
                    route.epoch,
                    *corr,
                    &roots[route.root_index],
                    &route.session,
                    storm_project_config(true, semantic_enabled, config_change, 0),
                );
                pending_binds.insert(
                    *corr,
                    BindTiming {
                        route: route.channel,
                        started_at: Instant::now(),
                        bound: timing.bind_bound,
                        measure_in_storm_stats: timing.measure_in_storm_stats,
                    },
                );
                route.next_bind_at =
                    now + Duration::from_millis(1_300 + u64::from(route.channel % 5) * 80);
            }

            let route_has_pending_tool = pending_tools
                .values()
                .any(|tool| tool.route == route.channel);
            let route_has_pending_bind = pending_binds
                .values()
                .any(|bind| bind.route == route.channel);
            if now >= route.next_tool_at && !route_has_pending_tool && !route_has_pending_bind {
                route.tool_count += 1;
                *corr += 1;
                send_interactive_tool(
                    tx,
                    route,
                    *corr,
                    &roots[route.root_index],
                    pending_completions,
                    timing,
                );
                let name = match route.tool_count % 4 {
                    0 => "read",
                    1 => "write",
                    2 => "subc_test_emit_bash_completed",
                    _ => "subc_test_enqueue_watcher_event",
                };
                pending_tools.insert(
                    *corr,
                    ToolTiming {
                        route: route.channel,
                        started_at: now,
                        name,
                        bound: timing.tool_bound,
                        measure_in_storm_stats: timing.measure_in_storm_stats,
                    },
                );
                route.next_tool_at = now + Duration::from_millis(500);
            }
        }

        if now >= next_health_at {
            *corr += 1;
            send_control(tx, *corr, ModuleControlRequest::HealthCheck {});
            pending_health.insert(
                *corr,
                HealthTiming {
                    started_at: now,
                    bound: timing.health_bound,
                    measure_in_storm_stats: timing.measure_in_storm_stats,
                },
            );
            next_health_at = now + Duration::from_millis(400);
        }

        read_one_or_sleep(
            rx,
            pending_binds,
            pending_tools,
            pending_health,
            pending_completions,
            stats,
            Duration::from_millis(25),
        )
        .await;
    }
}

async fn drive_heavy_init_saturation_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let executor = Arc::clone(&session.executor);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 1_500_u64;

    for (channel, root) in [(1_u16, &session.root1), (2_u16, &session.root2)] {
        send_bind(
            &tx,
            channel,
            corr,
            root,
            &format!("heavy-init-{channel}"),
            storm_project_config(false, false, false, 0),
        );
        expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;
        corr += 1;
    }

    assert_eq!(
        executor.heavy_permits(),
        1,
        "two-worker storm rigs reserve one worker for RouteBind admission"
    );
    let (started_tx, started_rx) = crossbeam_channel::bounded(2);
    let (release_tx, release_rx) = crossbeam_channel::bounded(2);
    let mut heavy_jobs = Vec::new();
    for (index, root) in [&session.root1, &session.root2].into_iter().enumerate() {
        let root_id = ProjectRootId::from_path(root).expect("bound heavy root id");
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        heavy_jobs.push(executor.submit(
            root_id,
            Lane::HeavyInit,
            format!("storm-heavy-init-{index}"),
            Box::new(move |_| {
                started_tx
                    .send(index)
                    .expect("signal injected HeavyInit delay");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release injected HeavyInit delay");
                Response::success(format!("storm-heavy-init-{index}"), json!({ "ok": true }))
            }),
        ));
    }
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("one HeavyInit job starts");
    assert!(
        started_rx.recv_timeout(Duration::from_millis(75)).is_err(),
        "HeavyInit storm must leave a worker free for a bind"
    );

    let fresh_root = tempfile::tempdir().expect("fresh bind root");
    corr += 1;
    send_bind(
        &tx,
        3,
        corr,
        fresh_root.path(),
        "heavy-init-fresh-bind",
        storm_project_config(false, false, false, 0),
    );
    expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;

    for _ in 0..heavy_jobs.len() {
        release_tx.send(()).expect("release HeavyInit storm job");
    }
    for heavy in heavy_jobs {
        heavy
            .recv_timeout(Duration::from_secs(2))
            .expect("HeavyInit storm completion");
    }
    send_goodbye_and_wait(&tx).await;
}

/// Deterministic many-standing-root storm: cold-build capacity is saturated by
/// held permits, more standing passes than maintenance workers are submitted,
/// and a PureRead with a finite deadline must still finish in budget. Yields
/// are asserted through the standing pass responses, pending depths must stay
/// under the configured caps, and standing work resumes after permits release.
async fn drive_standing_yield_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let executor = Arc::clone(&session.executor);
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 2_500_u64;

    for (channel, root) in [(1_u16, &session.root1), (2_u16, &session.root2)] {
        send_bind(
            &tx,
            channel,
            corr,
            root,
            &format!("standing-yield-{channel}"),
            storm_project_config(false, false, false, 0),
        );
        expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;
        corr += 1;
    }

    // Hold BOTH cold-build permits so every standing pass must yield.
    let permit_a = aft::cold_build_limiter::try_acquire().expect("hold cold permit A");
    let permit_b = aft::cold_build_limiter::try_acquire().expect("hold cold permit B");

    // Submit more standing passes than maintenance workers: every pass must
    // observe zero cold slots and yield without waiting.
    // Production standing passes submit to standing-root actors that the
    // StandingActor registered; this rig reuses the two bound roots to drive
    // the same MaintenanceCommit lane path.
    let passes = executor.pool_size() + 4;
    let mut receivers = Vec::with_capacity(passes);
    for index in 0..passes {
        let root = [&session.root1, &session.root2][index % 2].clone();
        let root_id = ProjectRootId::from_path(&root).expect("standing root id");
        // A pass runs as a plain maintenance job; the production standing pass
        // body yields via try_acquire_standing_with_limiter, which must return
        // None here because both permits are held by this test.
        let yielded = Arc::new(AtomicBool::new(false));
        let yielded_probe = Arc::clone(&yielded);
        let request_id = format!("storm-standing-yield-{index}");
        receivers.push((
            executor.submit_maintenance_async(
                root_id,
                Lane::MaintenanceCommit,
                request_id.clone(),
                Box::new(move |_| {
                    // Mirrors the production standing pass shape: an immediate,
                    // non-waiting cold-build attempt. This test holds both cold
                    // permits, so every pass must yield promptly and must never
                    // block a maintenance worker on cold admission.
                    let permit = aft::cold_build_limiter::try_acquire();
                    let yielded = permit.is_none();
                    if yielded {
                        yielded_probe.store(true, std::sync::atomic::Ordering::Release);
                    }
                    drop(permit);
                    Response::success(request_id, json!({ "yielded": yielded }))
                }),
            ),
            yielded,
        ));
    }

    // All passes complete (yield) even though cold slots stay saturated, and
    // every pass must actually observe the yield — both permits are held by
    // this test, so try_acquire cannot succeed.
    for (receiver, yielded) in receivers {
        let response = tokio::time::timeout(Duration::from_secs(5), receiver)
            .await
            .expect("standing pass yields instead of waiting")
            .expect("standing pass channel open");
        assert!(
            response.success,
            "a standing pass answers success without blocking on cold admission"
        );
        assert!(
            response.data.get("yielded").and_then(Value::as_bool) == Some(true),
            "standing pass must yield while both cold permits are held"
        );
        assert!(
            yielded.load(std::sync::atomic::Ordering::Acquire),
            "yield probe must confirm try_acquire returned None"
        );
    }

    // A PureRead with a finite deadline finishes well inside its budget after
    // standing passes yield under saturated cold capacity.
    let read_root_id = ProjectRootId::from_path(&session.root1).expect("read root id");
    let started = Instant::now();
    let (read_tx, read_rx) = tokio::sync::oneshot::channel();
    let _read_cancel = {
        let (rx, _cancellation) = executor.submit_cancellable_async_with_deadline(
            read_root_id,
            Lane::PureRead,
            "standing-yield-read".to_string(),
            Box::new(|_| Response::success("standing-yield-read", json!({ "read": true }))),
            Some(Instant::now() + Duration::from_secs(3)),
        );
        tokio::spawn(async move {
            let _ = read_tx.send(rx.await);
        });
    };
    let read_response = tokio::time::timeout(Duration::from_secs(3), read_rx)
        .await
        .expect("read finished inside its deadline")
        .expect("read channel open");
    assert!(
        read_response.is_ok() && read_response.unwrap().success,
        "PureRead completes while cold capacity is saturated"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "read latency stayed inside the request budget"
    );

    // The nonblocking health mirror remains observable without contention,
    // and pending depths stay under the configured caps.
    let liveness = executor
        .try_dispatch_liveness_snapshot()
        .expect("nonblocking dispatch liveness under standing saturation");
    assert!(
        liveness.interactive.queued <= liveness.interactive_queue_cap,
        "pending interactive depth stayed under the process cap"
    );
    assert!(
        liveness.maintenance.queued <= liveness.maintenance_queue_cap,
        "pending maintenance depth stayed under the process cap"
    );

    // Release the cold permits and confirm nonblocking cold admission recovers.
    drop(permit_a);
    drop(permit_b);
    let resumed = aft::cold_build_limiter::try_acquire();
    assert!(
        resumed.is_some(),
        "cold admission resumes after held permits release"
    );
    drop(resumed);

    send_goodbye_and_wait(&tx).await;
}

async fn drive_completion_saturation_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let (tx, mut rx) = start_io(session.stream);
    let (_dirs, roots) = create_storm_roots(2);
    let mut corr = 1_000_u64;
    send_bind(
        &tx,
        1,
        corr,
        &roots[0],
        "saturation-a",
        storm_project_config(false, false, false, 0),
    );
    expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;
    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_test_emit_bash_completed_burst",
        json!({ "prefix": "saturation", "count": 512 }),
    );
    corr += 1;
    let bind_corr = corr;
    send_bind(
        &tx,
        2,
        bind_corr,
        &roots[1],
        "saturation-b",
        storm_project_config(false, false, false, 0),
    );
    let started = Instant::now();
    loop {
        let frame = read_frame_from_rx(&mut rx, Duration::from_secs(10), "saturation ack").await;
        if is_ack(&frame, bind_corr) {
            let elapsed = started.elapsed();
            assert!(
                elapsed <= BIND_ACK_BOUND,
                "bind ack behind completion flood took {elapsed:?}"
            );
            break;
        }
    }
    send_goodbye_and_wait(&tx).await;
}

async fn drive_slow_config_read_daemon(input: FakeDaemonInput) {
    let _guard = EnvGuard::set("AFT_TEST_SUBC_CONFIG_READ_DELAY_MS", "750");
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let (tx, mut rx) = start_io(session.stream);
    let (_dirs, roots) = create_storm_roots(1);
    let corr = 2_000_u64;
    send_bind(
        &tx,
        1,
        corr,
        &roots[0],
        "slow-config-read",
        storm_project_config(false, false, false, 0),
    );
    let started = Instant::now();
    expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;
    assert!(
        started.elapsed() <= BIND_ACK_BOUND,
        "slow config read bind took {:?}",
        started.elapsed()
    );
    send_goodbye_and_wait(&tx).await;
}

async fn drive_detach_rebind_daemon(input: FakeDaemonInput) {
    // The lifecycle probe enables the route-detach delay only for this test
    // scenario. It makes an eager rebind race ahead of recording a completion
    // for replay, so the awaited events verify ordering without transport timing.
    let _detach_delay = EnvGuard::set("AFT_TEST_SUBC_ROUTE_DETACH_DELAY_MS", "200");
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let root = session.root1.clone();
    let state = Arc::clone(&session.state);
    let executor = Arc::clone(&session.executor);
    let mut lifecycle_events = session
        .lifecycle_events
        .expect("detach/rebind lifecycle probe installed");
    let (tx, mut rx) = start_io(session.stream);
    let mut corr = 3_000_u64;
    const SESSION: &str = "restart-session";
    const RELIABLE_TASK: &str = "restart-reliable-completion";

    send_bind(
        &tx,
        1,
        corr,
        &root,
        SESSION,
        storm_project_config(false, false, false, 0),
    );
    expect_ack_or_watchdog(&mut rx, corr).await;

    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "bash",
        json!({ "command": "sleep 1; printf storm-task-done", "background": true, "timeout": 10_000 }),
    );
    let launch = expect_tool_response_or_watchdog(&mut rx, corr).await;
    let task_id = launch["task_id"].as_str().expect("task_id").to_string();

    corr += 1;
    send_tool(
        &tx,
        1,
        corr,
        "subc_test_defer_bash_completed",
        json!({ "task_id": RELIABLE_TASK, "session_id": SESSION }),
    );
    expect_tool_response_or_watchdog(&mut rx, corr).await;
    send_route_goodbye(&tx, 1, corr + 10);
    expect_lifecycle_event(
        &mut lifecycle_events,
        SubcLifecycleEvent::RouteDetached {
            route_channel: 1,
            route_epoch: 1,
            session_id: SESSION.to_string(),
        },
    )
    .await;

    state.release_deferred_pushes();
    expect_lifecycle_event(
        &mut lifecycle_events,
        SubcLifecycleEvent::ReliableCompletionRetained {
            task_id: RELIABLE_TASK.to_string(),
            session_id: SESSION.to_string(),
        },
    )
    .await;

    corr += 20;
    send_bind(
        &tx,
        2,
        corr,
        &root,
        SESSION,
        storm_project_config(false, false, false, 0),
    );
    expect_ack_or_watchdog(&mut rx, corr).await;
    expect_lifecycle_event(
        &mut lifecycle_events,
        SubcLifecycleEvent::ReliableCompletionReplayed {
            route_channel: 2,
            route_epoch: 1,
            task_id: RELIABLE_TASK.to_string(),
            session_id: SESSION.to_string(),
        },
    )
    .await;

    wait_for_registered_bash_task(&executor, &root, &task_id, SESSION).await;
    send_goodbye_and_wait(&tx).await;
}

async fn drive_epoch_rebind_rejection_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let (tx, mut rx) = start_io(session.stream);
    let root = session.root1;
    let mut corr = 5_000_u64;
    const CHANNEL: u16 = 9;
    const INSTALLED_EPOCH: u32 = 2;

    send_bind_epoch(
        &tx,
        CHANNEL,
        INSTALLED_EPOCH,
        corr,
        &root,
        "storm-epoch-current",
        storm_project_config(false, false, false, 0),
    );
    expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;
    for epoch in [INSTALLED_EPOCH, INSTALLED_EPOCH - 1] {
        corr += 1;
        send_bind_epoch(
            &tx,
            CHANNEL,
            epoch,
            corr,
            &root,
            "storm-epoch-rejected",
            storm_project_config(false, false, false, 0),
        );
        let error = expect_error(&mut rx, corr, TOOL_BOUND).await;
        assert_eq!(
            error.get("code").and_then(Value::as_str),
            Some("config_divergence")
        );
    }

    corr += 1;
    send_tool_epoch(
        &tx,
        CHANNEL,
        INSTALLED_EPOCH,
        corr,
        "echo",
        json!({ "case": "fast" }),
    );
    let response = expect_tool_response(&mut rx, corr, TOOL_BOUND).await;
    assert_eq!(response["success"].as_bool(), Some(true));
    send_goodbye_and_wait(&tx).await;
}

async fn drive_route_bind_deadline_daemon(input: FakeDaemonInput) {
    let session = subc_bridge_test::open_fake_daemon_session(input).await;
    let (tx, mut rx) = start_io(session.stream);
    let (_dirs, roots) = create_storm_roots(1);
    let mut corr = 4_000_u64;
    send_bind(
        &tx,
        1,
        corr,
        &roots[0],
        "deadline-session",
        storm_project_config(false, false, false, 0),
    );
    expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;

    corr += 1;
    let slow_corr = corr;
    send_bind(
        &tx,
        2,
        slow_corr,
        &roots[0],
        "deadline-session",
        storm_project_config(
            false,
            false,
            true,
            ROUTE_BIND_DEADLINE.as_millis() as u64 + 750,
        ),
    );
    let started = Instant::now();
    let error = expect_error(
        &mut rx,
        slow_corr,
        ROUTE_BIND_DEADLINE + Duration::from_secs(3),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= ROUTE_BIND_DEADLINE && elapsed <= ROUTE_BIND_DEADLINE + Duration::from_secs(2),
        "deadline surfaced at {elapsed:?}, expected about {ROUTE_BIND_DEADLINE:?}"
    );
    assert_eq!(
        error.get("code").and_then(Value::as_str),
        Some("actor_not_ready")
    );

    tokio::time::sleep(Duration::from_secs(2)).await;
    corr += 1;
    send_bind_epoch(
        &tx,
        2,
        2,
        corr,
        &roots[0],
        "deadline-session",
        storm_project_config(false, false, false, 0),
    );
    expect_ack_within(&mut rx, corr, BIND_ACK_BOUND).await;
    send_goodbye_and_wait(&tx).await;
}

fn build_route_specs(scale: &StormScale, roots: &[PathBuf]) -> Vec<RouteSpec> {
    let mut routes = Vec::new();
    let now = Instant::now();
    for root_index in 0..roots.len() {
        for session_index in 0..scale.sessions_per_root {
            let channel = (routes.len() + 1) as u16;
            routes.push(RouteSpec {
                channel,
                epoch: 1,
                root_index,
                session: format!("storm-r{root_index}-s{session_index}"),
                next_bind_at: now + Duration::from_millis(1_300 + u64::from(channel % 5) * 80),
                next_tool_at: now + Duration::from_millis(200 + u64::from(channel % 3) * 75),
                bind_count: if root_index % 2 == 1 && session_index == 0 {
                    9
                } else {
                    0
                },
                tool_count: 0,
            });
        }
    }
    routes
}

fn create_parent_and_linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let fixture = tempfile::tempdir().expect("worktree storm fixture");
    let parent_root = fixture.path().join("parent");
    let worktree_root = fixture.path().join("linked-worktree");
    std::fs::create_dir_all(parent_root.join("src")).expect("parent source dir");
    std::fs::write(
        parent_root.join("src/lib.rs"),
        "pub fn root_alpha() -> usize { 1 }\npub fn root_beta() -> usize { root_alpha() + 1 }\n",
    )
    .expect("parent source");
    std::fs::write(parent_root.join("README.md"), "# parent root\n").expect("parent readme");
    init_git(&parent_root);

    let mut worktree = Command::new("git");
    assert!(
        crate::test_helpers::apply_hermetic_git_env(worktree.current_dir(&parent_root))
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&worktree_root)
            .arg("HEAD")
            .status()
            .expect("git worktree add")
            .success(),
        "failed to create linked git worktree"
    );

    (fixture, parent_root, worktree_root)
}

fn create_storm_roots(count: usize) -> (Vec<tempfile::TempDir>, Vec<PathBuf>) {
    let mut dirs = Vec::new();
    let mut paths = Vec::new();
    for index in 0..count {
        let dir = tempfile::tempdir().expect("storm root tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("storm src dir");
        std::fs::write(
            src.join("lib.rs"),
            format!(
                "pub fn root_{index}_alpha() -> usize {{ {index} }}\npub fn root_{index}_beta() -> usize {{ root_{index}_alpha() + 1 }}\n"
            ),
        )
        .expect("storm lib file");
        std::fs::write(
            dir.path().join("README.md"),
            format!("# storm root {index}\n"),
        )
        .expect("storm readme");
        init_git(dir.path());
        paths.push(dir.path().to_path_buf());
        dirs.push(dir);
    }
    (dirs, paths)
}

fn prepare_storm_root(root: &Path) {
    std::fs::write(root.join("README.md"), "storm regression fixture\n")
        .expect("storm fixture README");
}

fn write_storm_file(path: &Path, marker: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("storm fixture parent");
    }
    std::fs::write(path, format!("pub fn marker() {{ /* {marker} */ }}\n"))
        .expect("storm fixture file");
}

async fn bind_ready_search_root(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: &mut u64,
    root: &Path,
    session: &str,
) {
    *corr += 1;
    send_bind(
        tx,
        1,
        *corr,
        root,
        session,
        storm_project_config(true, false, false, 0),
    );
    expect_ack_within(rx, *corr, SETUP_BIND_BOUND).await;
    wait_for_ready_health(tx, rx, corr, &[root.to_path_buf()], &HashSet::new()).await;
}

async fn collect_tool_responses(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    mut expected: HashSet<u64>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while !expected.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        assert!(
            !remaining.is_zero(),
            "timed out waiting for tool responses: {expected:?}"
        );
        let frame = read_frame_from_rx(rx, remaining, "concurrent tool responses").await;
        if !expected.contains(&frame.header.corr) {
            continue;
        }
        assert_eq!(
            frame.header.ty,
            FrameType::Response,
            "tool corr {} returned {:?}",
            frame.header.corr,
            frame.header.ty
        );
        let body: Value = serde_json::from_slice(&frame.body).expect("concurrent tool body");
        assert_ne!(
            body.get("isError").and_then(Value::as_bool),
            Some(true),
            "concurrent tool failed: {body:?}"
        );
        expected.remove(&frame.header.corr);
    }
}

async fn watcher_progress(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: &mut u64,
) -> Value {
    *corr += 1;
    send_tool(tx, 1, *corr, "subc_storm_watcher_pending", json!({}));
    expect_tool_response(rx, *corr, TOOL_BOUND).await
}

async fn watcher_pending(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: &mut u64,
) -> usize {
    watcher_progress(tx, rx, corr)
        .await
        .get("pending")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

async fn wait_for_watcher_empty(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: &mut u64,
) {
    let deadline = Instant::now() + CONCURRENCY_SCENARIO_BOUND;
    loop {
        if watcher_pending(tx, rx, corr).await == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "watcher backlog did not drain");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_grep_total(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: &mut u64,
    pattern: &str,
    expected: u64,
) {
    let deadline = Instant::now() + CONCURRENCY_SCENARIO_BOUND;
    loop {
        *corr += 1;
        send_tool(tx, 1, *corr, "grep", json!({ "pattern": pattern }));
        let response = expect_tool_response(rx, *corr, TOOL_BOUND).await;
        if response.get("total_matches").and_then(Value::as_u64) == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "grep {pattern:?} did not reach {expected} matches: {response:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn init_git(root: &Path) {
    let mut init = Command::new("git");
    assert!(
        crate::test_helpers::apply_hermetic_git_env(init.current_dir(root))
            .args(["init", "--quiet"])
            .status()
            .expect("git init")
            .success()
    );
    let mut add = Command::new("git");
    assert!(
        crate::test_helpers::apply_hermetic_git_env(add.current_dir(root))
            .args(["add", "."])
            .status()
            .expect("git add")
            .success()
    );
    let mut commit = Command::new("git");
    assert!(
        crate::test_helpers::apply_hermetic_git_env(commit.current_dir(root))
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "initial"
            ])
            .status()
            .expect("git commit")
            .success()
    );
}

fn touch_between_rebinds(root: &Path, count: usize) {
    std::fs::write(
        root.join("src").join(format!("storm_touch_{count}.rs")),
        format!("pub fn storm_touch_{count}() -> usize {{ {count} }}\n"),
    )
    .expect("write storm touch file");
}

fn write_user_semantic_config(path: &Path, base_url: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("user config parent");
    }
    std::fs::write(
        path,
        json!({
            "semantic": {
                "backend": "openai_compatible",
                "model": "storm-embedding",
                "base_url": base_url,
                "timeout_ms": 60_000,
                "max_batch_size": 64,
                "max_files": 1_000,
            }
        })
        .to_string(),
    )
    .expect("write user semantic config");
}

fn artifact_storm_config(storage_dir: &Path) -> Value {
    json!({
        "storage_dir": storage_dir,
        "search_index": true,
        "semantic_search": false,
        "callgraph_store": true,
        "inspect": { "enabled": true },
        "tool_surface": "all",
    })
}

fn storm_project_config(
    search: bool,
    semantic: bool,
    changed: bool,
    configure_sleep_ms: u64,
) -> Value {
    let mut doc = json!({
        "search_index": search,
        "semantic_search": semantic,
        "callgraph_store": false,
        "inspect": { "enabled": changed },
        "tool_surface": "all",
    });
    if configure_sleep_ms > 0 {
        doc["subc_test_configure_sleep_ms"] = json!(configure_sleep_ms);
    }
    doc
}

fn send_interactive_tool(
    tx: &mpsc::UnboundedSender<Frame>,
    route: &RouteSpec,
    corr: u64,
    root: &Path,
    pending_completions: &mut HashMap<String, CompletionTiming>,
    timing: StormTiming,
) {
    match route.tool_count % 4 {
        0 => send_tool_epoch(
            tx,
            route.channel,
            route.epoch,
            corr,
            "read",
            json!({ "filePath": "README.md", "limit": 20 }),
        ),
        1 => send_tool_epoch(
            tx,
            route.channel,
            route.epoch,
            corr,
            "write",
            json!({
                "filePath": format!("storm_session_{}.txt", route.channel),
                "content": format!("route={} tool={}\n", route.channel, route.tool_count),
            }),
        ),
        2 => {
            let task_id = format!("storm-completion-{}-{}", route.channel, route.tool_count);
            pending_completions.insert(
                task_id.clone(),
                CompletionTiming {
                    started_at: Instant::now(),
                    bound: timing.completion_bound,
                    measure_in_storm_stats: timing.measure_in_storm_stats,
                },
            );
            send_tool_epoch(
                tx,
                route.channel,
                route.epoch,
                corr,
                "subc_test_emit_bash_completed",
                json!({ "task_id": task_id, "session_id": route.session }),
            );
        }
        _ => {
            let _ = root;
            send_tool_epoch(
                tx,
                route.channel,
                route.epoch,
                corr,
                "subc_test_enqueue_watcher_event",
                json!({}),
            );
        }
    }
}

fn start_io(
    stream: tokio::net::TcpStream,
) -> (mpsc::UnboundedSender<Frame>, mpsc::UnboundedReceiver<Frame>) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Frame>();
    let (mut read_half, mut write_half) = stream.into_split();
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write_frame(&mut write_half, &frame).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        loop {
            match read_frame(&mut read_half).await {
                Ok(Some(frame)) => {
                    if in_tx.send(frame).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => panic!("storm io read failed: {error}"),
            }
        }
    });
    (out_tx, in_rx)
}

fn send_bind(
    tx: &mpsc::UnboundedSender<Frame>,
    channel: u16,
    corr: u64,
    root: &Path,
    session: &str,
    doc: Value,
) {
    send_bind_epoch(tx, channel, 1, corr, root, session, doc);
}

fn send_bind_epoch(
    tx: &mpsc::UnboundedSender<Frame>,
    channel: u16,
    epoch: u32,
    corr: u64,
    root: &Path,
    session: &str,
    doc: Value,
) {
    let project_cfg = root.join(".cortexkit").join("aft.jsonc");
    std::fs::create_dir_all(project_cfg.parent().expect("project cfg parent")).expect("cfg dir");
    std::fs::write(&project_cfg, serde_json::to_string(&doc).expect("cfg json"))
        .expect("write cfg");
    let request = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity {
            project_root: root.to_path_buf(),
            harness: "opencode".to_string(),
            session: session.to_string(),
        },
        consumer_capabilities: None,
        principal: Some(Principal::Direct),
        admission_facts: Default::default(),
    };
    send_control(tx, corr, request);
}

async fn write_measure_bind(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    root: &Path,
    session: &str,
) {
    let project_cfg = root.join(".cortexkit").join("aft.jsonc");
    std::fs::create_dir_all(project_cfg.parent().expect("measurement config parent"))
        .expect("create measurement config directory");
    std::fs::write(
        &project_cfg,
        serde_json::to_string(&storm_project_config(false, false, false, 0))
            .expect("measurement config json"),
    )
    .expect("write measurement config");
    let corr = 700_000 + u64::from(channel);
    let request = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity {
            project_root: root.to_path_buf(),
            harness: "opencode".to_string(),
            session: session.to_string(),
        },
        consumer_capabilities: None,
        principal: Some(Principal::Direct),
        admission_facts: Default::default(),
    };
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&request).expect("measurement bind body"),
    )
    .expect("measurement bind frame");
    write_frame(stream, &frame)
        .await
        .expect("write measurement bind");

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(12), read_frame(stream))
            .await
            .expect("measurement bind timeout")
            .expect("measurement bind frame read")
            .expect("measurement stream closed during bind");
        if frame.header.ty == FrameType::Response
            && frame.header.channel == 0
            && frame.header.corr == corr
        {
            break;
        }
    }
}

fn build_measure_tool_frame(
    channel: u16,
    corr: u64,
    payload_bytes: usize,
    push_count: usize,
) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        1,
        corr,
        serde_json::to_vec(&json!({
            "name": "subc_storm_egress_payload",
            "arguments": {
                "payload_bytes": payload_bytes,
                "push_count": push_count,
            },
        }))
        .expect("measurement tool body"),
    )
    .expect("measurement tool frame")
}

fn print_egress_summary(observations: &[EgressObservation], push_frames: usize) {
    let mut requested_sizes = observations
        .iter()
        .map(|observation| observation.requested_bytes)
        .collect::<Vec<_>>();
    requested_sizes.sort_unstable();
    requested_sizes.dedup();
    for requested_bytes in requested_sizes {
        let class = observations
            .iter()
            .filter(|observation| observation.requested_bytes == requested_bytes)
            .collect::<Vec<_>>();
        let mut latencies = class
            .iter()
            .map(|observation| observation.latency.as_secs_f64() * 1_000.0)
            .collect::<Vec<_>>();
        latencies.sort_by(f64::total_cmp);
        let p50 = latencies[(latencies.len() - 1) / 2];
        let p95 = latencies[((latencies.len() - 1) * 95) / 100];
        let max = latencies[latencies.len() - 1];
        let mean_frame_bytes = class
            .iter()
            .map(|observation| observation.frame_bytes as u64)
            .sum::<u64>()
            / class.len() as u64;
        eprintln!(
            "EGRESS_SUMMARY requested_bytes={} count={} mean_frame_bytes={} p50_ms={p50:.3} p95_ms={p95:.3} max_ms={max:.3} push_frames={push_frames}",
            requested_bytes,
            class.len(),
            mean_frame_bytes,
        );
    }
}

fn send_control(tx: &mpsc::UnboundedSender<Frame>, corr: u64, request: ModuleControlRequest) {
    tx.send(
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            corr,
            serde_json::to_vec(&request).expect("control body"),
        )
        .expect("control frame"),
    )
    .expect("send control frame");
}

fn send_tool(
    tx: &mpsc::UnboundedSender<Frame>,
    channel: u16,
    corr: u64,
    name: &str,
    arguments: Value,
) {
    send_tool_epoch(tx, channel, 1, corr, name, arguments);
}

fn send_tool_epoch(
    tx: &mpsc::UnboundedSender<Frame>,
    channel: u16,
    epoch: u32,
    corr: u64,
    name: &str,
    arguments: Value,
) {
    tx.send(
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            epoch,
            corr,
            serde_json::to_vec(&json!({ "name": name, "arguments": arguments }))
                .expect("tool body"),
        )
        .expect("tool frame"),
    )
    .expect("send tool frame");
}

fn send_route_goodbye(tx: &mpsc::UnboundedSender<Frame>, channel: u16, corr: u64) {
    tx.send(
        Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Passive, false),
            channel,
            1,
            corr,
            Vec::new(),
        )
        .expect("route goodbye"),
    )
    .expect("send route goodbye");
}

async fn send_goodbye_and_wait(tx: &mpsc::UnboundedSender<Frame>) {
    send_goodbye(tx);
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn send_goodbye(tx: &mpsc::UnboundedSender<Frame>) {
    tx.send(
        Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            999_999,
            Vec::new(),
        )
        .expect("goodbye"),
    )
    .expect("send goodbye");
}

async fn read_one_or_sleep(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    pending_binds: &mut HashMap<u64, BindTiming>,
    pending_tools: &mut HashMap<u64, ToolTiming>,
    pending_health: &mut HashMap<u64, HealthTiming>,
    pending_completions: &mut HashMap<String, CompletionTiming>,
    stats: &mut StormStats,
    timeout: Duration,
) {
    if let Ok(Some(frame)) = tokio::time::timeout(timeout, rx.recv()).await {
        process_storm_frame(
            frame,
            pending_binds,
            pending_tools,
            pending_health,
            pending_completions,
            stats,
        );
    }
}

async fn drain_until_idle(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    pending_binds: &mut HashMap<u64, BindTiming>,
    pending_tools: &mut HashMap<u64, ToolTiming>,
    pending_health: &mut HashMap<u64, HealthTiming>,
    pending_completions: &mut HashMap<String, CompletionTiming>,
    stats: &mut StormStats,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline
        && (!pending_binds.is_empty()
            || !pending_tools.is_empty()
            || !pending_health.is_empty()
            || !pending_completions.is_empty())
    {
        read_one_or_sleep(
            rx,
            pending_binds,
            pending_tools,
            pending_health,
            pending_completions,
            stats,
            Duration::from_millis(100),
        )
        .await;
    }
    assert!(
        pending_binds.is_empty(),
        "pending binds did not drain: {pending_binds:?}"
    );
    assert!(
        pending_tools.is_empty(),
        "pending tools did not drain: {pending_tools:?}"
    );
    assert!(
        pending_health.is_empty(),
        "pending health checks did not drain: {pending_health:?}"
    );
    assert!(
        pending_completions.is_empty(),
        "pending completions did not drain: {pending_completions:?}"
    );
}

fn process_storm_frame(
    frame: Frame,
    pending_binds: &mut HashMap<u64, BindTiming>,
    pending_tools: &mut HashMap<u64, ToolTiming>,
    pending_health: &mut HashMap<u64, HealthTiming>,
    pending_completions: &mut HashMap<String, CompletionTiming>,
    stats: &mut StormStats,
) {
    match frame.header.ty {
        FrameType::Response if frame.header.channel == 0 => {
            let response: ModuleControlResponse =
                serde_json::from_slice(&frame.body).expect("control response");
            if matches!(response, ModuleControlResponse::RouteBindAck {}) {
                let timing = pending_binds
                    .remove(&frame.header.corr)
                    .expect("unexpected bind ack");
                let elapsed = timing.started_at.elapsed();
                assert!(
                    elapsed <= timing.bound,
                    "RouteBindAck for route {} took {elapsed:?}, exceeding {:?}",
                    timing.route,
                    timing.bound
                );
                if timing.measure_in_storm_stats {
                    stats.bind_latencies.push(elapsed);
                }
            } else if let Some(report) = response.health_report() {
                let timing = pending_health
                    .remove(&frame.header.corr)
                    .expect("unexpected health response");
                let elapsed = timing.started_at.elapsed();
                assert!(
                    elapsed <= timing.bound,
                    "health.check took {elapsed:?}, exceeding {:?}",
                    timing.bound
                );
                if timing.measure_in_storm_stats {
                    stats.max_health_latency = stats.max_health_latency.max(elapsed);
                    assert_pending_bind_age(&report);
                }
            }
        }
        FrameType::Response => {
            let timing = pending_tools
                .remove(&frame.header.corr)
                .expect("unexpected tool response");
            let elapsed = timing.started_at.elapsed();
            assert!(
                elapsed <= timing.bound,
                "{} on route {} took {elapsed:?}, exceeding {:?}",
                timing.name,
                timing.route,
                timing.bound
            );
            if timing.measure_in_storm_stats {
                stats.max_tool_latency = stats.max_tool_latency.max(elapsed);
            }
            let body: Value = serde_json::from_slice(&frame.body).expect("tool response body");
            assert_ne!(
                body.get("isError").and_then(Value::as_bool),
                Some(true),
                "tool failed: {body:?}"
            );
        }
        FrameType::Push => {
            if let Some(task_id) = push_task_id(&frame) {
                if let Some(timing) = pending_completions.remove(task_id.as_str()) {
                    let elapsed = timing.started_at.elapsed();
                    assert!(
                        elapsed <= timing.bound,
                        "completion {task_id} push took {elapsed:?}, exceeding {:?}",
                        timing.bound
                    );
                    if timing.measure_in_storm_stats {
                        stats.max_completion_latency = stats.max_completion_latency.max(elapsed);
                    }
                }
            }
        }
        FrameType::Error => {
            let body: Value = serde_json::from_slice(&frame.body).expect("error body");
            panic!(
                "unexpected error frame on channel {} corr {}: {body:?}",
                frame.header.channel, frame.header.corr
            );
        }
        other => panic!("unexpected storm frame: {other:?}"),
    }
}

fn assert_pending_bind_age(report: &HealthReport) {
    let Some(metrics) = report.metrics.as_ref() else {
        return;
    };
    let Some(age) = metrics
        .pointer("/dispatch_path/pending_binds/oldest_age_ms")
        .and_then(Value::as_u64)
    else {
        return;
    };
    assert!(
        age <= BIND_ACK_BOUND.as_millis() as u64,
        "pending_binds.oldest_age_ms exceeded bind bound: {age}"
    );
}

async fn wait_for_ready_health(
    tx: &mpsc::UnboundedSender<Frame>,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: &mut u64,
    roots: &[PathBuf],
    semantic_roots: &HashSet<usize>,
) {
    // Readiness is an observable state transition, not a performance assertion. A
    // generous deadline keeps cold index builds valid under full-suite I/O contention;
    // each health response still proves the module is live while the poll waits.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        *corr += 1;
        send_control(tx, *corr, ModuleControlRequest::HealthCheck {});
        let response =
            read_control_response(rx, *corr, Duration::from_secs(5), "ready health").await;
        let report = response.health_report().expect("health report");
        if health_has_ready_roots(&report, roots, semantic_roots) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "roots did not become ready: {report:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn health_has_ready_roots(
    report: &HealthReport,
    roots: &[PathBuf],
    semantic_roots: &HashSet<usize>,
) -> bool {
    let Some(metrics) = report.metrics.as_ref() else {
        return false;
    };
    let Some(entries) = metrics.get("roots").and_then(Value::as_array) else {
        return false;
    };
    for (index, root) in roots.iter().enumerate() {
        let expected = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let expected = strip_verbatim(&expected.to_string_lossy());
        let Some(entry) = entries.iter().find(|entry| {
            entry
                .get("project_root")
                .and_then(Value::as_str)
                .map(strip_verbatim)
                .as_deref()
                == Some(expected.as_str())
        }) else {
            return false;
        };
        if entry.get("state").and_then(Value::as_str) != Some("ready") {
            return false;
        }
        if entry
            .pointer("/search_index/status")
            .and_then(Value::as_str)
            != Some("ready")
        {
            return false;
        }
        let expected_semantic = if semantic_roots.contains(&index) {
            "ready"
        } else {
            "disabled"
        };
        if entry
            .pointer("/semantic_index/status")
            .and_then(Value::as_str)
            != Some(expected_semantic)
        {
            return false;
        }
    }
    true
}

fn strip_verbatim(path: &str) -> String {
    path.trim_start_matches(r"\\?\").to_string()
}

fn assert_bind_stats(latencies: &[Duration]) {
    assert!(
        !latencies.is_empty(),
        "storm produced no bind acknowledgements"
    );
    let max = *latencies.iter().max().expect("max bind latency");
    let mut sorted = latencies.to_vec();
    sorted.sort();
    let p99_index = sorted
        .len()
        .saturating_sub(1)
        .min((sorted.len() * 99) / 100);
    let p99 = sorted[p99_index];
    assert!(
        max <= BIND_ACK_BOUND,
        "max bind latency {max:?} exceeded {BIND_ACK_BOUND:?}; p99={p99:?}"
    );
}

async fn expect_lifecycle_event(
    events: &mut mpsc::UnboundedReceiver<SubcLifecycleEvent>,
    expected: SubcLifecycleEvent,
) {
    let actual = events
        .recv()
        .await
        .expect("detach/rebind lifecycle probe closed");
    assert_eq!(actual, expected, "unexpected detach/rebind lifecycle event");
}

async fn expect_ack_or_watchdog(rx: &mut mpsc::UnboundedReceiver<Frame>, corr: u64) {
    loop {
        let frame = rx.recv().await.expect("EOF waiting for RouteBindAck");
        if is_ack(&frame, corr) {
            return;
        }
    }
}

async fn expect_tool_response_or_watchdog(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: u64,
) -> Value {
    loop {
        let frame = rx.recv().await.expect("EOF waiting for tool response");
        if frame.header.corr != corr {
            continue;
        }
        if frame.header.ty == FrameType::Error {
            let body: Value = serde_json::from_slice(&frame.body).expect("tool error body");
            panic!("tool corr {corr} returned error frame: {body:?}");
        }
        if frame.header.ty == FrameType::Response {
            let body: Value = serde_json::from_slice(&frame.body).expect("tool body");
            assert_ne!(
                body.get("isError").and_then(Value::as_bool),
                Some(true),
                "tool failed: {body:?}"
            );
            return body.get("structuredContent").cloned().unwrap_or(body);
        }
    }
}

async fn wait_for_registered_bash_task(
    executor: &Executor,
    root: &Path,
    task_id: &str,
    session_id: &str,
) {
    let root_id = ProjectRootId::from_path(root).expect("bound restart root id");
    loop {
        let ctx = executor
            .actor_context(&root_id)
            .expect("rebound route retains its actor context");
        let storage_dir = ctx.config().storage_dir.clone();
        let snapshot = ctx
            .bash_background()
            .status(
                task_id,
                session_id,
                Some(root),
                storage_dir.as_deref(),
                2_048,
            )
            .expect("background task remains registered after rebind");
        if snapshot.info.status.is_terminal() {
            assert_eq!(
                snapshot.info.status,
                aft::bash_background::BgTaskStatus::Completed,
                "detached background task should complete successfully: {snapshot:?}"
            );
            assert_eq!(
                snapshot.exit_code,
                Some(0),
                "detached background task should preserve its successful exit: {snapshot:?}"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn expect_ack_within(rx: &mut mpsc::UnboundedReceiver<Frame>, corr: u64, bound: Duration) {
    let started = Instant::now();
    loop {
        let frame = read_frame_from_rx(rx, bound, "RouteBindAck").await;
        if is_ack(&frame, corr) {
            let elapsed = started.elapsed();
            assert!(elapsed <= bound, "RouteBindAck {corr} took {elapsed:?}");
            return;
        }
    }
}

async fn expect_tool_response(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: u64,
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    let mut skipped = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        assert!(
            !remaining.is_zero(),
            "timed out waiting for tool response corr {corr}; skipped frames: {skipped:?}"
        );
        let frame = read_frame_from_rx(rx, remaining, "tool response").await;
        if frame.header.corr != corr {
            skipped.push(format!(
                "{:?}/ch{}/corr{}",
                frame.header.ty, frame.header.channel, frame.header.corr
            ));
            continue;
        }
        if frame.header.ty == FrameType::Error {
            let body: Value = serde_json::from_slice(&frame.body).expect("tool error body");
            panic!("tool corr {corr} returned error frame: {body:?}");
        }
        if frame.header.ty == FrameType::Response {
            let body: Value = serde_json::from_slice(&frame.body).expect("tool body");
            assert_ne!(
                body.get("isError").and_then(Value::as_bool),
                Some(true),
                "tool failed: {body:?}"
            );
            return body.get("structuredContent").cloned().unwrap_or(body);
        }
    }
}

async fn expect_error(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: u64,
    timeout: Duration,
) -> Value {
    loop {
        let frame = read_frame_from_rx(rx, timeout, "error frame").await;
        if frame.header.ty == FrameType::Error && frame.header.corr == corr {
            return serde_json::from_slice::<Value>(&frame.body).expect("error body");
        }
    }
}

async fn read_control_response(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    corr: u64,
    timeout: Duration,
    label: &str,
) -> ModuleControlResponse {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        assert!(!remaining.is_zero(), "timed out waiting for {label}");
        let frame = read_frame_from_rx(rx, remaining, label).await;
        if frame.header.ty == FrameType::Response
            && frame.header.channel == 0
            && frame.header.corr == corr
        {
            return serde_json::from_slice(&frame.body).expect("control response body");
        }
    }
}

async fn read_frame_from_rx(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    timeout: Duration,
    label: &str,
) -> Frame {
    tokio::time::timeout(timeout, rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .unwrap_or_else(|| panic!("EOF waiting for {label}"))
}

fn is_ack(frame: &Frame, corr: u64) -> bool {
    if frame.header.ty != FrameType::Response
        || frame.header.channel != 0
        || frame.header.corr != corr
    {
        return false;
    }
    serde_json::from_slice::<ModuleControlResponse>(&frame.body)
        .is_ok_and(|response| matches!(response, ModuleControlResponse::RouteBindAck {}))
}

fn push_task_id(frame: &Frame) -> Option<String> {
    let body: Value = serde_json::from_slice(&frame.body).ok()?;
    body.get("task_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}
