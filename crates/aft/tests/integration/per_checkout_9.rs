//! Multi-repo parent folders with views on: a session opened in a plain
//! folder of repositories builds no index of its own and answers grep, glob,
//! `aft_search`, the call graph tools and inspect from its children's own
//! published indexes, loaded once and kept warm.
//!
//! The fixtures build every child index through the real configure path of a
//! session bound to that child, then drop that session, so the parent reads
//! exactly what a child's own sessions leave on disk. Loading child indexes
//! inside each call is the failure mode these tests guard against, together
//! with its measured symptoms: per-call loads, children reported stale when
//! unchanged, stores refused because of SQLite journal files, a large child's
//! semantic index refused by a size cap, and parent latency far above a
//! child's.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

const DEADLINE: Duration = Duration::from_secs(180);
const MODEL: &str = "parent-folder-mock";
const COMMON: &str = "PARENT_FOLDER_COMMON_TOKEN";
const PARENT_SEARCH_WATCHDOG_DEADLINE: Duration = Duration::from_secs(400);
const PARENT_SEARCH_WATCHDOG_SELF_TEST_ENV: &str = "AFT_PARENT_SEARCH_WATCHDOG_SELF_TEST";

enum ParentSearchWatchdogMessage {
    EnterPhase { name: String, at: Instant },
    Stop { at: Instant },
}

struct ParentSearchWatchdog {
    sender: std::sync::mpsc::Sender<ParentSearchWatchdogMessage>,
    handle: Option<thread::JoinHandle<()>>,
}

fn abort_parent_search_watchdog(
    timeout: Duration,
    phase: &str,
    phase_elapsed: Duration,
    total_elapsed: Duration,
) -> ! {
    let _ = writeln!(
        std::io::stderr().lock(),
        "parent search phase watchdog timed out after {timeout:?} in phase `{phase}` (phase elapsed {:.3}s, total elapsed {:.3}s); aborting the process",
        phase_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64()
    );
    std::process::abort()
}

impl ParentSearchWatchdog {
    fn new(timeout: Duration) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(0);
        let handle = thread::Builder::new()
            .name("parent-search-phase-watchdog".into())
            .spawn(move || {
                let _ = ready_sender.send(());
                let Ok(ParentSearchWatchdogMessage::EnterPhase { name, at }) = receiver.recv()
                else {
                    return;
                };
                let mut phase = name;
                let mut phase_started = at;
                let watchdog_started = at;
                let deadline = watchdog_started + timeout;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match receiver.recv_timeout(remaining) {
                        Ok(ParentSearchWatchdogMessage::EnterPhase { name, at }) => {
                            let elapsed = at.saturating_duration_since(phase_started);
                            let total_elapsed = at.saturating_duration_since(watchdog_started);
                            if total_elapsed >= timeout {
                                abort_parent_search_watchdog(
                                    timeout,
                                    &phase,
                                    elapsed,
                                    total_elapsed,
                                );
                            }
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "parent search watchdog: phase `{phase}` elapsed {:.3}s",
                                elapsed.as_secs_f64()
                            );
                            phase = name;
                            phase_started = at;
                        }
                        Ok(ParentSearchWatchdogMessage::Stop { at }) => {
                            let total_elapsed = at.saturating_duration_since(watchdog_started);
                            if total_elapsed >= timeout {
                                abort_parent_search_watchdog(
                                    timeout,
                                    &phase,
                                    at.saturating_duration_since(phase_started),
                                    total_elapsed,
                                );
                            }
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "parent search watchdog: phase `{phase}` elapsed {:.3}s (total {:.3}s)",
                                at.saturating_duration_since(phase_started).as_secs_f64(),
                                total_elapsed.as_secs_f64()
                            );
                            return;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let phase_elapsed = phase_started.elapsed();
                            let total_elapsed = watchdog_started.elapsed();
                            abort_parent_search_watchdog(
                                timeout,
                                &phase,
                                phase_elapsed,
                                total_elapsed,
                            );
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn parent search phase watchdog");
        ready_receiver
            .recv()
            .expect("parent search phase watchdog started");
        Self {
            sender,
            handle: Some(handle),
        }
    }

    fn enter(&self, phase: &str) {
        self.sender
            .send(ParentSearchWatchdogMessage::EnterPhase {
                name: phase.to_owned(),
                at: Instant::now(),
            })
            .expect("parent search phase watchdog is running");
    }
}

impl Drop for ParentSearchWatchdog {
    fn drop(&mut self) {
        let _ = self
            .sender
            .send(ParentSearchWatchdogMessage::Stop { at: Instant::now() });
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn fast_refresh() {
    aft::views::parent::set_default_refresh_interval(Duration::from_millis(100));
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", message]);
}

fn init_repo(root: &Path, files: &[(&str, String)]) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "parent@example.test"]);
    git(root, &["config", "user.name", "Parent Test"]);
    for (path, text) in files {
        write(root, path, text);
    }
    commit_all(root, "initial");
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).unwrap()
}

fn data(response: Response) -> Value {
    response.data
}

/// Deterministic embeddings over HTTP for the semantic tests.
struct MockEmbedder {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbedder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            while thread_running.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                thread::spawn(move || {
                    let _ = serve(&mut stream);
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            handle: Some(handle),
        }
    }
}

impl Drop for MockEmbedder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn vector(text: &str) -> Vec<f32> {
    blake3::hash(text.as_bytes()).as_bytes()[..8]
        .iter()
        .map(|byte| f32::from(*byte) / 255.0 - 0.5)
        .collect()
}

fn serve(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut body_start = None;
    let mut length = 0usize;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if body_start.is_none() {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                body_start = Some(end + 4);
                for line in String::from_utf8_lossy(&bytes[..end]).lines() {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
        }
        if body_start.is_some_and(|start| bytes.len() >= start + length) {
            break;
        }
    }
    let body = body_start
        .and_then(|start| bytes.get(start..start + length))
        .and_then(|body| serde_json::from_slice::<Value>(body).ok())
        .unwrap_or_else(|| json!({ "input": [] }));
    let inputs = match &body["input"] {
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        Value::String(value) => vec![value.clone()],
        _ => Vec::new(),
    };
    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| json!({ "embedding": vector(input), "index": index }))
        .collect::<Vec<_>>();
    let body = json!({ "data": data }).to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[derive(Clone, Copy)]
struct Planes {
    trigram: bool,
    callgraph: bool,
    semantic: bool,
}

const TRIGRAM_ONLY: Planes = Planes {
    trigram: true,
    callgraph: false,
    semantic: false,
};

fn config_doc(views: bool, planes: Planes, embedder: Option<&MockEmbedder>) -> Value {
    let mut doc = json!({
        "indexes": {
            "trigram": planes.trigram,
            "semantic": planes.semantic,
            "callgraph": planes.callgraph,
        },
        "views": { "enabled": views },
    });
    if let Some(embedder) = embedder {
        doc["semantic"] = json!({
            "backend": "openai_compatible",
            "model": MODEL,
            "base_url": embedder.base_url,
            "timeout_ms": 5_000,
            "max_batch_size": 64,
            "max_files": 2_000
        });
    }
    doc
}

/// A child session that builds only its trigram index. The child's trigram
/// artifact is the same `cache.bin` with views on or off; views stay off here
/// so dozens of fixture children do not each queue a view publication that
/// the trigram tests never read.
fn child_trigram_doc() -> Value {
    config_doc(false, TRIGRAM_ONLY, None)
}

fn configure(root: &Path, storage: &Path, doc: Value) -> Arc<AppContext> {
    let ctx = Arc::new(
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        )
        .with_test_checkpoint_store(),
    );
    let configured = aft::commands::configure::handle_configure(
        &request(json!({
            "id": "configure-parent-folder",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(doc),
        })),
        &ctx,
    );
    assert!(configured.success, "configure failed: {configured:?}");
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    ctx
}

fn drain(ctx: &AppContext) {
    // Match the runtime maintenance tick: a busy bind can defer identity
    // resolution or view publication beyond configure's first drain.
    aft::runtime_drain::drain_deferred_configure_maintenance(ctx);
    aft::runtime_drain::drain_watcher_events(ctx);
    aft::runtime_drain::drain_semantic_index_events(ctx);
    aft::runtime_drain::drain_semantic_refresh_events(ctx);
}

/// Asks the child session for its call graph until it answers. The first
/// answer binds the child's v3 call-graph view and publishes it; a later one
/// publishes the edits made since.
fn child_callgraph_answer(ctx: &AppContext, root: &Path) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let answer = aft::commands::callers::handle_callers(
            &request(json!({
                "id": "child-callers",
                "command": "callers",
                "file": root.join("src/lib.rs"),
                "symbol": "target",
            })),
            ctx,
        );
        if answer.success {
            return data(answer);
        }
        assert!(
            Instant::now() < deadline,
            "child call graph view never answered: {}: {:?}",
            root.display(),
            answer.data
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn grep(ctx: &AppContext, pattern: &str) -> Value {
    data(aft::commands::grep::handle_grep(
        &request(json!({
            "id": "parent-grep",
            "command": "grep",
            "pattern": pattern,
            "max_results": 5_000,
        })),
        ctx,
    ))
}

fn glob(ctx: &AppContext, pattern: &str) -> Value {
    data(aft::commands::glob::handle_glob(
        &request(json!({"id": "parent-glob", "command": "glob", "pattern": pattern})),
        ctx,
    ))
}

/// `(path relative to base, line)` for every grep match.
fn grep_rows(answer: &Value, base: &Path, prefix: &Path) -> BTreeSet<(String, u64)> {
    answer["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("grep failed: {answer:#}"))
        .iter()
        .map(|row| {
            let file = PathBuf::from(row["file"].as_str().unwrap());
            let relative = file.strip_prefix(base).unwrap_or(&file).to_path_buf();
            (
                prefix.join(relative).to_string_lossy().replace('\\', "/"),
                row["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn glob_rows(answer: &Value, base: &Path, prefix: &Path) -> BTreeSet<String> {
    answer["files"]
        .as_array()
        .unwrap_or_else(|| panic!("glob failed: {answer:#}"))
        .iter()
        .map(|file| {
            let file = PathBuf::from(file.as_str().unwrap());
            let relative = file.strip_prefix(base).unwrap_or(&file).to_path_buf();
            prefix.join(relative).to_string_lossy().replace('\\', "/")
        })
        .collect()
}

fn gap_kinds(answer: &Value) -> Vec<String> {
    answer["gaps"]
        .as_array()
        .map(|gaps| {
            gaps.iter()
                .map(|gap| gap["kind"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn child_files(index: usize) -> Vec<(&'static str, String)> {
    vec![
        (
            "src/lib.rs",
            format!(
                "pub fn unique_needle_{index}() -> u32 {{\n    {index}\n}}\n\n// {COMMON} in lib {index}\n"
            ),
        ),
        ("src/util.rs", format!("pub fn helper() {{}}\n// {COMMON}\n")),
        ("notes.txt", format!("notes for repository {index}\n")),
    ]
}

struct Folder {
    _temp: tempfile::TempDir,
    root: PathBuf,
    storage: PathBuf,
    children: Vec<PathBuf>,
}

fn folder(count: usize) -> Folder {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let children = (0..count)
        .map(|index| root.join(format!("repo-{index:02}")))
        .collect::<Vec<_>>();
    for (index, child) in children.iter().enumerate() {
        init_repo(child, &child_files(index));
    }
    // A file directly in the parent folder belongs to no child repository.
    write(
        &root,
        "README.md",
        &format!("{COMMON} in the parent folder\n"),
    );
    Folder {
        _temp: temp,
        root,
        storage,
        children,
    }
}

/// Median of `runs` timings of `query`, in milliseconds.
fn median_ms(runs: usize, mut query: impl FnMut()) -> f64 {
    let mut samples = (0..runs)
        .map(|_| {
            let started = Instant::now();
            query();
            started.elapsed().as_secs_f64() * 1000.0
        })
        .collect::<Vec<_>>();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// The parent session `ctx` serves from, once its first load finished.
fn parent_session(ctx: &AppContext) -> Arc<aft::views::parent::ParentSession> {
    let session = started_parent_session(ctx);
    assert!(
        session.wait_rounds(1, DEADLINE),
        "parent folder never finished its first load"
    );
    session
}

/// The parent session `ctx` serves from, as soon as it exists; its worker may
/// still be discovering or loading the children.
///
/// A bind that finds aft.db busy (here, usually the fixture children's
/// background maintenance still writing the shared storage) settles no
/// identity: configure turns views off, starts no parent session, and arms a
/// background retry that replays the bind once the database is free. Both
/// runtimes poll that retry from the configure-tail maintenance drain, so this
/// runs the same drain until the replay starts the session. Any other reason
/// for a missing session fails at once, and so does a retry that never lands
/// within the deadline.
fn started_parent_session(ctx: &AppContext) -> Arc<aft::views::parent::ParentSession> {
    let started = Instant::now();
    let mut retried = false;
    let session = loop {
        aft::runtime_drain::drain_deferred_configure_maintenance(ctx);
        if let Some(session) = aft::views::parent::session_for(ctx) {
            break session;
        }
        let degraded = ctx.degraded_reasons();
        let busy_bind_retrying = degraded
            .iter()
            .any(|reason| reason.contains("aft.db was busy at bind"));
        assert!(
            busy_bind_retrying && started.elapsed() < DEADLINE,
            "parent session active: none after {:?}; views={} degraded={degraded:?}",
            started.elapsed(),
            ctx.config().views.enabled,
        );
        retried = true;
        thread::sleep(Duration::from_millis(20));
    };
    if retried {
        eprintln!(
            "parent session started by the busy-database bind retry after {} ms",
            started.elapsed().as_millis()
        );
    }
    session
}

/// Waits until the parent's grep for `pattern` names no child as unavailable:
/// with the data layer, each child's checkout is built on the first query that
/// reaches it, and until it publishes the child is named as still loading.
fn settled_grep(ctx: &AppContext, pattern: &str) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let answer = grep(ctx, pattern);
        if !gap_kinds(&answer).contains(&"parent_child_unavailable".to_string()) {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "the children never published: {answer:#}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The watcher reports a batch it lost (an overflowing event queue) as
/// `RescanRequired`, with no paths. An edit inside that batch must still reach
/// the parent's grep once the rescan the event asks for has run. The data
/// layer hears only the watcher's changed-path batches, so it never rescans
/// and the child checkout keeps serving the old file.
#[test]
fn parent_rescan_after_lost_watcher_events_reaches_child_edits() {
    fast_refresh();
    let folder = folder(2);
    std::fs::remove_file(folder.root.join("README.md")).unwrap();
    let parent = configure(
        &folder.root,
        &folder.storage,
        config_doc(true, TRIGRAM_ONLY, None),
    );
    let _session = parent_session(&parent);
    settled_grep(&parent, COMMON);

    let (tx, rx) = crossbeam_channel::unbounded();
    *parent.watcher_rx().lock() = Some(rx);
    write(
        &folder.children[1],
        "src/lost.rs",
        "pub fn lost() {}\n// LOST_BATCH_PARENT_EDIT_NEEDLE\n",
    );
    tx.send(WatcherDispatchEvent::RescanRequired(
        aft::watcher_filter::RescanReason::BufferOverflow,
    ))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        aft::runtime_drain::drain_watcher_events(&parent);
        let answer = grep(&parent, "LOST_BATCH_PARENT_EDIT_NEEDLE");
        if grep_rows(&answer, &folder.root, Path::new(""))
            == BTreeSet::from([("repo-01/src/lost.rs".to_string(), 2)])
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the rescan never reached the edit: {answer:#}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// A call-graph command routed by the parent until it answers: the first one
/// that reaches a child builds the child's checkout, and until it publishes
/// the child is named as still loading.
fn settled_route(parent: &AppContext, payload: Value) -> Response {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let answer = aft::views::parent::route(&request(payload.clone()), parent)
            .expect("the command is routed for a parent folder");
        if answer.success {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "the parent never answered {payload}: {:?}",
            answer.data
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The call graph is served from each child's checkout in the data layer:
/// answers carry the child prefix and are current, so they name no
/// publication they are as of; a child no session ever bound answers once its
/// checkout is built; a file outside every child is a named gap; inspect fans
/// out to what the children's own sessions stored; and an AFT edit in a child
/// reaches the parent's next answer.
#[test]
fn parent_callgraph_answers_from_child_checkouts_and_follows_their_edits() {
    fast_refresh();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    let graph = root.join("graph");
    let plain = root.join("plain");
    init_repo(
        &graph,
        &[(
            "src/lib.rs",
            "pub fn target() -> u32 {\n    1\n}\n\npub fn caller() -> u32 {\n    target()\n}\n"
                .to_string(),
        )],
    );
    init_repo(&plain, &[("src/lib.rs", "pub fn alone() {}\n".to_string())]);
    write(&root, "loose.rs", "pub fn loose() {}\n");
    let with_graph = Planes {
        trigram: true,
        callgraph: true,
        semantic: false,
    };
    // `plain` is never bound to a session of its own.
    let child = configure(&graph, &storage, config_doc(true, with_graph, None));
    child_callgraph_answer(&child, &graph);

    let parent = configure(&root, &storage, config_doc(true, with_graph, None));
    let _session = parent_session(&parent);
    let callers_request = json!({
        "id": "parent-callers",
        "command": "callers",
        "file": "graph/src/lib.rs",
        "symbol": "target",
    });
    let callers = data(settled_route(&parent, callers_request.clone()));
    let text = callers.to_string();
    assert!(text.contains("graph/src/lib.rs"), "{callers:#}");
    assert!(text.contains("caller"), "{callers:#}");
    assert!(callers.get("as_of").is_none(), "{callers:#}");

    let plain_callers = settled_route(
        &parent,
        json!({
            "id": "parent-callers-plain",
            "command": "callers",
            "file": "plain/src/lib.rs",
            "symbol": "alone",
        }),
    );
    assert!(
        plain_callers.data.get("gaps").is_none(),
        "{:?}",
        plain_callers.data
    );
    let outside = aft::views::parent::route(
        &request(json!({
            "id": "parent-callers-outside",
            "command": "callers",
            "file": "loose.rs",
            "symbol": "loose",
        })),
        &parent,
    )
    .unwrap();
    assert_eq!(
        outside.data["gaps"][0]["kind"],
        "outside_child_repositories"
    );

    // Inspect fans out too: a child that never ran a project-wide inspect and
    // the per-file categories are named gaps; nothing is computed for them.
    let inspected = aft::views::parent::route(
        &request(json!({
            "id": "parent-inspect",
            "command": "inspect",
            "sections": ["dead_code", "todos"],
        })),
        &parent,
    )
    .unwrap();
    assert!(inspected.success);
    assert_eq!(inspected.data["complete"], false);
    assert!(inspected.data["summary"]["dead_code"].is_object());
    let reasons = inspected.data["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gap| {
            format!(
                "{} {}",
                gap["path"].as_str().unwrap(),
                gap["reason"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert!(
        reasons.contains(
            &"graph dead_code: no current inspect results for this repository yet".to_string()
        ),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(
            &"plain todos: computed only by a session opened in this repository".to_string()
        ),
        "{reasons:?}"
    );

    // A parent-folder inspect honours the same category switches without
    // turning missing child aggregates into incomplete disabled categories.
    parent.update_config(|config| {
        config.inspect.categories.dead_code = false;
        config.inspect.categories.todos = false;
    });
    let off = aft::views::parent::route(
        &request(json!({
            "id":"parent-inspect-off", "command":"inspect", "scope":"graph", "sections":["dead_code","todos"]
        })),
        &parent,
    )
    .unwrap();
    assert_eq!(off.data["complete"], true, "{}", off.data);
    assert!(off.data["text"]
        .as_str()
        .unwrap()
        .contains("dead code: off (inspect.categories.dead_code)"));
    assert_eq!(off.data["summary"]["todos"]["off"], true);
    assert_eq!(parent.inspect_manager().reuse_start_count_for_test(), 0);

    // An AFT write through the child's session: the parent's checkout of the
    // child records it too, and its next answer has the new caller.
    let written = aft::commands::write::handle_write(
        &request(json!({
            "id": "child-write",
            "command": "write",
            "file": graph.join("src/lib.rs"),
            "content": "pub fn target() -> u32 {\n    2\n}\n\npub fn caller() -> u32 {\n    target()\n}\n\npub fn second() -> u32 {\n    target()\n}\n",
        })),
        &child,
    );
    assert!(written.success, "{:?}", written.data);
    let followed = data(settled_route(&parent, callers_request));
    assert!(followed.to_string().contains("second"), "{followed:#}");
    assert!(followed.get("as_of").is_none(), "{followed:#}");
    // The parent built and wrote no call graph of its own.
    assert!(parent.callgraph_store().read().unwrap().is_none());
    drop(child);
}

#[test]
fn parent_search_phase_watchdog_aborts_with_phase() {
    if std::env::var_os(PARENT_SEARCH_WATCHDOG_SELF_TEST_ENV).is_some() {
        let watchdog = ParentSearchWatchdog::new(Duration::from_millis(300));
        watchdog.enter("injected blocked phase");
        thread::sleep(Duration::from_millis(1500));
        return;
    }

    let output = Command::new(std::env::current_exe().expect("test binary path"))
        .args([
            "--exact",
            "per_checkout_9::parent_search_phase_watchdog_aborts_with_phase",
            "--nocapture",
        ])
        .env(PARENT_SEARCH_WATCHDOG_SELF_TEST_ENV, "1")
        .output()
        .expect("run watchdog self-test child");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "watchdog child should abort; stderr: {stderr}"
    );
    assert!(
        stderr.contains("timed out after 300ms in phase `injected blocked phase`"),
        "watchdog child did not report its blocked phase: {stderr}"
    );
}

/// `aft_search` merges the children's semantic views by score under the
/// parent prefix, even when a child also has a legacy semantic artifact larger
/// than the borrowed reader's size cap: the parent reads the child's
/// per-checkout view, which has no such cap.
#[test]
fn parent_search_merges_child_semantic_views_without_a_size_cap() {
    let watchdog = ParentSearchWatchdog::new(PARENT_SEARCH_WATCHDOG_DEADLINE);
    watchdog.enter("fixture setup");
    fast_refresh();
    let embedder = MockEmbedder::start();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    init_repo(
        &alpha,
        &[(
            "src/retry.rs",
            "pub fn retry_with_backoff(attempts: u32) -> u64 {\n    2u64.pow(attempts)\n}\n"
                .to_string(),
        )],
    );
    init_repo(
        &beta,
        &[(
            "src/cache.rs",
            "pub struct LruCache {\n    capacity: usize,\n}\n\nimpl LruCache {\n    pub fn evict_oldest(&mut self) -> Option<String> {\n        None\n    }\n}\n"
                .to_string(),
        )],
    );
    let semantic = Planes {
        trigram: true,
        callgraph: false,
        semantic: true,
    };
    // The child sessions stay alive so they fold their fills into published
    // generations, which is what the parent serves.
    let mut sessions = Vec::new();
    for (name, child) in [("alpha", &alpha), ("beta", &beta)] {
        watchdog.enter(&format!("configure child {name}"));
        let ctx = configure(child, &storage, config_doc(true, semantic, Some(&embedder)));
        watchdog.enter(&format!("fill child {name} semantic view"));
        let deadline = Instant::now() + DEADLINE;
        loop {
            drain(&ctx);
            let filled = ctx.checkout_semantic_runtime().is_some_and(|runtime| {
                runtime
                    .search(&vector("probe"), 1, &|_| true)
                    .is_ok_and(|answer| answer.complete() && !answer.results.is_empty())
            });
            if filled {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child semantic view never filled"
            );
            thread::sleep(Duration::from_millis(20));
        }
        sessions.push(ctx);
    }
    // A legacy semantic artifact above the borrowed reader's 64 MiB cap. The
    // file is sparse, so it costs no disk space.
    let key = aft::search_index::artifact_cache_key(&alpha);
    let legacy = storage.join("semantic").join(&key).join("semantic.bin");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::File::create(&legacy)
        .unwrap()
        .set_len(65 * 1024 * 1024)
        .unwrap();

    watchdog.enter("configure parent");
    let parent = configure(&root, &storage, config_doc(true, semantic, Some(&embedder)));
    let session = parent_session(&parent);
    watchdog.enter("wait for parent semantic views");
    let deadline = Instant::now() + DEADLINE;
    while !session
        .children()
        .iter()
        .all(|child| child.semantic_ready())
    {
        assert!(
            Instant::now() < deadline,
            "children semantic views never loaded"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let search = || {
        watchdog.enter("parent route search");
        data(
            aft::views::parent::route(
                &request(json!({
                    "id": "parent-search",
                    "command": "semantic_search",
                    "query": "retry with exponential backoff",
                    "top_k": 10,
                })),
                &parent,
            )
            .unwrap(),
        )
    };
    // Until a child folds its fills into a published generation, the parent
    // names that child's unreflected files as a gap; it never fills them.
    let mut answer = search();
    while answer["complete"] != true {
        assert!(
            Instant::now() < deadline,
            "parent search never became complete: {answer:#}"
        );
        for ctx in &sessions {
            drain(ctx);
        }
        thread::sleep(Duration::from_millis(50));
        answer = search();
    }
    let rows = |answer: &Value| -> Vec<(String, String, u64)> {
        answer["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{answer:#}"))
            .iter()
            .map(|row| {
                (
                    row["file"].as_str().unwrap().to_owned(),
                    row["name"].as_str().unwrap_or_default().to_owned(),
                    row["start_line"].as_u64().unwrap_or(0),
                )
            })
            .collect()
    };
    let parent_rows = rows(&answer);
    let files = parent_rows
        .iter()
        .map(|(file, _, _)| file.clone())
        .collect::<BTreeSet<_>>();
    assert!(files.contains("alpha/src/retry.rs"), "{answer:#}");
    assert!(files.contains("beta/src/cache.rs"), "{answer:#}");
    assert_eq!(answer["complete"], true, "{answer:#}");
    // The same envelope and paging fields a normal aft_search answer has.
    for key in [
        "status",
        "text",
        "query",
        "include_tests",
        "interpreted_as",
        "query_kind",
        "result_count",
        "more_available",
        "engine_capped",
        "fully_degraded",
        "semantic_status",
        "results_list_envelope",
    ] {
        assert!(answer.get(key).is_some(), "missing {key}: {answer:#}");
    }

    // Each child's rows keep the order a direct search of that child gives:
    // the parent merges the children's own engine rankings.
    for (session_ctx, (child_root, prefix)) in
        sessions.iter().zip([(&alpha, "alpha"), (&beta, "beta")])
    {
        watchdog.enter("handle_semantic_search");
        let direct = aft::commands::semantic_search::handle_semantic_search(
            &request(json!({
                "id": "child-search",
                "command": "semantic_search",
                "query": "retry with exponential backoff",
                "top_k": 10,
            })),
            session_ctx,
        )
        .data;
        let direct = rows(&direct)
            .into_iter()
            .map(|(file, name, line)| {
                let file = Path::new(&file)
                    .strip_prefix(child_root)
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .unwrap_or(file);
                (format!("{prefix}/{file}"), name, line)
            })
            .collect::<Vec<_>>();
        let from_parent = parent_rows
            .iter()
            .filter(|(file, _, _)| file.starts_with(&format!("{prefix}/")))
            .cloned()
            .collect::<Vec<_>>();
        assert!(!from_parent.is_empty());
        assert!(
            direct.starts_with(&from_parent),
            "parent order {from_parent:?} disagrees with the child's own {direct:?}"
        );
    }

    // Paging: offset and topK select a window of the same merged ranking.
    watchdog.enter("compare precomputed child semantic query");
    precomputed_query_vector_ranks_byte_identically(&sessions[0], "retry with exponential backoff");
    watchdog.enter("parent route search");
    let page = data(
        aft::views::parent::route(
            &request(json!({
                "id": "parent-search-page",
                "command": "semantic_search",
                "query": "retry with exponential backoff",
                "topK": 1,
                "offset": 1,
            })),
            &parent,
        )
        .unwrap(),
    );
    assert_eq!(rows(&page), parent_rows[1..2].to_vec(), "{page:#}");
    assert_eq!(page["more_available"], parent_rows.len() > 2, "{page:#}");
    assert_eq!(page["results_list_envelope"]["shown"], 1, "{page:#}");

    drop(session);
    watchdog.enter("drop parent context");
    drop(parent);
    watchdog.enter("drop child beta context");
    drop(sessions.pop().unwrap());
    watchdog.enter("drop child alpha context");
    drop(sessions.pop().unwrap());
    watchdog.enter("drop temporary directory");
    drop(temp);
    watchdog.enter("drop mock embedder");
    drop(embedder);
}

/// A search given the query's own embedding as a precomputed vector answers
/// exactly like the normal path that embeds it: same ranked rows, same text.
fn precomputed_query_vector_ranks_byte_identically(ctx: &AppContext, query: &str) {
    let search = || {
        aft::commands::semantic_search::handle_semantic_search(
            &request(json!({
                "id": "precomputed",
                "command": "semantic_search",
                "query": query,
                "top_k": 10,
            })),
            ctx,
        )
        .data
    };
    let normal = search();
    let vector = {
        let mut model = ctx.semantic_embedding_model().lock();
        let model = model.as_mut().expect("the normal search started the model");
        model
            .embed_query_cached(
                query,
                aft::semantic_index::QueryBudget::from_config(&ctx.config().semantic),
            )
            .unwrap()
    };
    // No model in the slot: the precomputed vector must be the only source.
    let model = ctx.semantic_embedding_model().lock().take();
    let precomputed =
        aft::commands::semantic_search::with_precomputed_query_vector(query, vector, search);
    assert!(ctx.semantic_embedding_model().lock().is_none());
    *ctx.semantic_embedding_model().lock() = model;
    assert!(!normal["results"].as_array().unwrap().is_empty());
    assert_eq!(
        serde_json::to_string(&normal["results"]).unwrap(),
        serde_json::to_string(&precomputed["results"]).unwrap()
    );
    assert_eq!(normal["text"], precomputed["text"]);
}

/// Discovery runs on the session worker, never on the configure path: with
/// a walk held until the test releases it, the bind still returns. Queries
/// name discovery as a gap meanwhile; the children appear once it ends.
///
/// The bind runs on its own thread and must return while discovery is still
/// held. A bind that waited for discovery could never return before the
/// release, so the deadline only bounds how long the test waits to say so; it
/// is not a speed bound, which a loaded runner can exceed for reasons that
/// have nothing to do with discovery.
#[test]
fn slow_discovery_never_delays_the_bind() {
    fast_refresh();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    for index in 0..20 {
        std::fs::create_dir_all(root.join(format!("repo-{index:02}")).join(".git")).unwrap();
    }
    let discovery = aft::views::parent::hold_discovery_for_test(&root);
    let ctx = Arc::new(
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        )
        .with_test_checkpoint_store(),
    );
    let configure = request(json!({
        "id": "configure-slow-parent",
        "command": "configure",
        "harness": "opencode",
        "project_root": root,
        "storage_dir": storage,
        "config": crate::helpers::user_config(config_doc(true, TRIGRAM_ONLY, None)),
    }));
    let (bound_tx, bound_rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let binder = {
        let ctx = Arc::clone(&ctx);
        thread::spawn(move || {
            let _ = bound_tx.send(aft::commands::configure::handle_configure(&configure, &ctx));
        })
    };
    let configured = bound_rx.recv_timeout(DEADLINE).unwrap_or_else(|_| {
        panic!("the bind did not return within {DEADLINE:?} while discovery was held")
    });
    let bind = started.elapsed();
    // The result has arrived, so the binder thread is only returning.
    binder.join().unwrap();
    assert!(configured.success, "{configured:?}");
    eprintln!(
        "parent folder bind with discovery held answered in {} ms",
        bind.as_millis()
    );
    // Discovery is held, so the first load cannot finish: wait only for the
    // session to exist, not for `parent_session`'s first load.
    let session = started_parent_session(&ctx);
    assert!(
        discovery.wait_until_started(DEADLINE),
        "discovery never started"
    );
    assert!(session.discovery().is_none(), "discovery escaped its hold");
    let first = grep(&ctx, "anything");
    let discovered_at = Instant::now() + DEADLINE;
    assert!(
        gap_kinds(&first).contains(&"parent_discovering".to_string()),
        "{first:#}"
    );
    drop(discovery);
    while session.discovery().is_none() {
        assert!(Instant::now() < discovered_at, "discovery never finished");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.children().len(), 20);
    let after = grep(&ctx, "anything");
    assert!(!gap_kinds(&after).contains(&"parent_discovering".to_string()));
    // Each child is named as loading only until its own checkout publishes,
    // then answered, so every child is eventually searched.
    let answered_by = Instant::now() + DEADLINE;
    loop {
        let reply = grep(&ctx, "anything");
        let unavailable = gap_kinds(&reply)
            .iter()
            .filter(|kind| *kind == "parent_child_unavailable")
            .count();
        if unavailable == 0 {
            break;
        }
        assert!(
            Instant::now() < answered_by,
            "{unavailable} children never answered: {reply:#}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Pi uses this stdin/stdout loop, not a daemon actor. Opening the view's
/// stores and reading its manifest is startup work too, even when publication
/// and the semantic worker are already detached.
#[cfg(feature = "test-timing-hooks")]
#[test]
fn standalone_view_open_never_blocks_requests() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("repo");
    let storage = base.join("storage");
    let started_file = base.join("view-open.started");
    let sweep_started = base.join("view-sweep.started");
    let names = (0..500)
        .map(|i| format!("src/file{i}.rs"))
        .collect::<Vec<_>>();
    let files = names
        .iter()
        .map(|name| (name.as_str(), "pub fn marker() {}\n".to_owned()))
        .collect::<Vec<_>>();
    init_repo(&root, &files);
    let mut aft = crate::helpers::AftProcess::spawn_with_env(&[
        ("AFT_TEST_VIEW_OPEN_DELAY_MS", std::ffi::OsStr::new("3000")),
        ("AFT_TEST_VIEW_OPEN_START_FILE", started_file.as_os_str()),
        ("AFT_TEST_VIEW_SWEEP_DELAY_MS", std::ffi::OsStr::new("3000")),
        ("AFT_TEST_VIEW_SWEEP_START_FILE", sweep_started.as_os_str()),
    ]);
    assert_eq!(aft.send(&json!({
        "id": "cfg", "command": "configure", "harness": "pi",
        "project_root": root, "storage_dir": storage,
        "config": crate::helpers::user_config(config_doc(true, Planes { trigram: true, callgraph: true, semantic: false }, None)),
    }).to_string())["success"], true);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !started_file.exists() {
        assert!(Instant::now() < deadline, "view open seam never reached");
        thread::sleep(Duration::from_millis(10));
    }
    let mut latencies = Vec::new();
    for req in [
        json!({"id": "read", "command": "tool_call", "name": "read", "arguments": {"filePath": root.join("src/file0.rs")}}),
        json!({"id": "drain", "command": "bash_drain_completions", "session_id": "startup"}),
        json!({"id": "status", "command": "status"}),
    ] {
        let sent = Instant::now();
        let response = aft.send(&req.to_string());
        latencies.push(sent.elapsed().as_millis());
        assert_eq!(response["success"], true, "{response:#}");
    }
    eprintln!("500-file standalone view-open I/O delay=3000ms: tool_call/drain/status latencies={latencies:?}ms");
    assert!(
        latencies.iter().all(|ms| *ms < 1000),
        "requests waited on view startup: {latencies:?}ms"
    );
    let sent = Instant::now();
    let graph = aft.send(&json!({"id": "graph", "command": "callers", "file": root.join("src/file0.rs"), "symbol": "marker"}).to_string());
    assert_eq!(graph["code"], "callgraph_building", "{graph:#}");
    assert!(
        sent.elapsed() < Duration::from_secs(1),
        "a loading graph waited for startup"
    );
    while !sweep_started.exists() {
        assert!(Instant::now() < deadline, "view sweep seam never reached");
        thread::sleep(Duration::from_millis(150));
    }
    let sent = Instant::now();
    assert_eq!(
        aft.send(&json!({"id": "sweep-drain", "command": "bash_drain_completions"}).to_string())
            ["success"],
        true
    );
    eprintln!(
        "standalone completion drain during 3000ms view sweep: {}ms",
        sent.elapsed().as_millis()
    );
    assert!(
        sent.elapsed() < Duration::from_secs(1),
        "completion drain waited for the view sweep"
    );
    assert!(aft.shutdown().success());
}

/// The 30 s callgraph start backstop and the semantic view disk load are on
/// the semantic worker, not stdin. Hold the manifest open past that backstop
/// to sample real requests in both phases, without a local model download.
#[cfg(feature = "test-timing-hooks")]
#[test]
fn standalone_semantic_grace_and_load_never_block_requests() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("repo");
    let storage = base.join("storage");
    let open_started = base.join("open.started");
    let load_started = base.join("semantic.started");
    let names = (0..500)
        .map(|i| format!("src/file{i}.rs"))
        .collect::<Vec<_>>();
    let files = names
        .iter()
        .map(|name| (name.as_str(), "pub fn marker() {}\n".to_owned()))
        .collect::<Vec<_>>();
    init_repo(&root, &files);
    let embedder = MockEmbedder::start();
    let mut aft = crate::helpers::AftProcess::spawn_with_semantic_env(&[
        ("AFT_TEST_VIEW_OPEN_DELAY_MS", std::ffi::OsStr::new("35000")),
        ("AFT_TEST_VIEW_OPEN_START_FILE", open_started.as_os_str()),
        (
            "AFT_TEST_SEMANTIC_VIEW_LOAD_DELAY_MS",
            std::ffi::OsStr::new("5000"),
        ),
        (
            "AFT_TEST_SEMANTIC_VIEW_LOAD_START_FILE",
            load_started.as_os_str(),
        ),
    ]);
    let started = Instant::now();
    assert_eq!(aft.send(&json!({
        "id": "cfg", "command": "configure", "harness": "pi",
        "project_root": root, "storage_dir": storage,
        "config": crate::helpers::user_config(config_doc(true, Planes { trigram: true, callgraph: true, semantic: true }, Some(&embedder))),
    }).to_string())["success"], true);
    let mut grace_max = 0;
    let mut load_max = 0;
    let mut grace_samples = 0;
    let mut load_samples = 0;
    while !load_started.exists() || load_samples < 3 {
        assert!(
            started.elapsed() < Duration::from_secs(50),
            "semantic load seam never reached"
        );
        let loading = load_started.exists();
        for req in [
            json!({"id": "read", "command": "tool_call", "name": "read", "arguments": {"filePath": root.join("src/file0.rs")}}),
            json!({"id": "drain", "command": "bash_drain_completions", "session_id": "startup"}),
            json!({"id": "status", "command": "status"}),
        ] {
            let sent = Instant::now();
            let response = aft.send(&req.to_string());
            let ms = sent.elapsed().as_millis();
            assert_eq!(response["success"], true, "{response:#}");
            if loading {
                load_max = load_max.max(ms);
            } else {
                grace_max = grace_max.max(ms);
            }
            if req["command"] == "status" {
                assert_eq!(
                    response["semantic_index"]["status"], "loading",
                    "{response:#}"
                );
            }
        }
        if loading {
            load_samples += 1;
        } else {
            grace_samples += 1;
        }
        thread::sleep(Duration::from_millis(150));
    }
    assert!(open_started.exists());
    assert!(grace_samples > 0 && load_samples >= 3);
    assert!(
        started.elapsed() >= Duration::from_secs(30),
        "production grace was not exercised"
    );
    eprintln!("500-file standalone: 30s grace {grace_samples} samples max={grace_max}ms; 5000ms semantic disk load {load_samples} samples max={load_max}ms");
    assert!(grace_max < 1000 && load_max < 1000);
    // The lane must eventually be usable, not merely hidden forever as loading.
    loop {
        let response = aft.send(&json!({"id": "ready", "command": "status"}).to_string());
        if response["semantic_index"]["status"] == "ready" {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "semantic lane never became ready: {response:#}"
        );
        thread::sleep(Duration::from_millis(150));
    }
    assert!(aft.shutdown().success());
}

/// Views stay off by default; with views off a parent folder keeps today's
/// behaviour and no parent session starts.
#[test]
fn views_off_folder_is_not_a_parent_session() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    init_repo(&root.join("child"), &child_files(0));
    let ctx = configure(&root, &storage, config_doc(false, TRIGRAM_ONLY, None));
    assert!(aft::views::parent::session_for(&ctx).is_none());
    assert!(aft::views::parent::session_for_root(&root).is_none());
    let answer = grep(&ctx, COMMON);
    assert!(!gap_kinds(&answer)
        .iter()
        .any(|kind| kind.starts_with("parent") || kind == "outside_child_repositories"));
    assert!(!Config::default().views.enabled);
}

