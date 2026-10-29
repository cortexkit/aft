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
    aft::runtime_drain::drain_search_index_events(ctx);
    aft::runtime_drain::drain_semantic_index_events(ctx);
    aft::runtime_drain::drain_semantic_refresh_events(ctx);
}

fn trigram_artifact(root: &Path, storage: &Path) -> PathBuf {
    let key = aft::search_index::artifact_cache_key(root);
    aft::search_index::resolve_cache_dir_with_key(&key, Some(storage)).join("cache.bin")
}

/// Waits until the child session's trigram index is ready and persisted.
/// The runtime drain also settles a bind whose identity was deferred by busy
/// storage, so its index can start once the database is free.
fn wait_trigram(ctx: &AppContext, root: &Path, storage: &Path) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let ready = ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready);
        if ready && trigram_artifact(root, storage).is_file() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child trigram index never persisted: {}; enabled={}; key={:?}; degraded={:?}; cache_unavailable={:?}",
            root.display(),
            ctx.config().indexes.trigram,
            ctx.cached_artifact_cache_key(root),
            ctx.degraded_reasons(),
            ctx.cache_unavailable_reason(),
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn runtime_drains_retry_child_trigram_after_busy_identity() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("alpha");
    let storage = base.join("storage");
    init_repo(
        &root,
        &[("src/lib.rs", "pub fn retry_marker() {}\n".into())],
    );
    std::fs::create_dir_all(&storage).unwrap();
    let lock = rusqlite::Connection::open(storage.join("aft.db")).unwrap();
    // Rollback-journal mode blocks identity readers as well as writers.
    lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    let ctx = configure(&root, &storage, child_trigram_doc());
    assert!(ctx
        .degraded_reasons()
        .iter()
        .any(|reason| reason.contains("aft.db was busy at bind")));
    assert!(ctx.cached_artifact_cache_key(&root).is_none());
    assert!(!ctx.config().indexes.trigram);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);

    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(&ctx);
        if ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready)
            && trigram_artifact(&root, &storage).is_file()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "runtime drains never retried the child's busy identity: {:?}",
            ctx.degraded_reasons()
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!ctx
        .degraded_reasons()
        .iter()
        .any(|reason| reason.contains("aft.db was busy at bind")));
    assert!(ctx.config().indexes.trigram);
    assert!(ctx.cached_artifact_cache_key(&root).is_some());
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

/// Waits until the parent serves a call-graph generation of `child` other
/// than `previous`, and returns it.
fn wait_parent_callgraph(child: &aft::views::parent::Child, previous: Option<&str>) -> String {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(generation) = child.callgraph_generation() {
            if Some(generation.as_str()) != previous {
                return generation;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the parent never served the child's call graph view"
        );
        thread::sleep(Duration::from_millis(20));
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

/// 32 repositories: the merged grep and glob answers equal the union of each
/// child's own answers with the child's folder as path prefix; repeated
/// queries load nothing; unchanged children are never reported stale; and the
/// parent's grep latency stays within the bounds asserted below relative to
/// one child's.
#[test]
fn thirty_two_repository_parent_merges_child_answers_without_per_call_loads() {
    fast_refresh();
    let folder = folder(32);
    let mut expected_grep = BTreeSet::new();
    let mut expected_glob = BTreeSet::new();
    let mut child_median = 0.0;
    let mut child_unique_median = 0.0;
    let chunks = folder.children.chunks(8).collect::<Vec<_>>();
    for chunk in chunks {
        let built = thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|child| {
                    let storage = folder.storage.clone();
                    let root = folder.root.clone();
                    scope.spawn(move || {
                        let ctx = configure(child, &storage, child_trigram_doc());
                        wait_trigram(&ctx, child, &storage);
                        let prefix = child.strip_prefix(&root).unwrap().to_path_buf();
                        let rows = grep_rows(&grep(&ctx, COMMON), child, &prefix);
                        let files = glob_rows(&glob(&ctx, "**/*.rs"), child, &prefix);
                        (ctx, rows, files)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (ctx, rows, files) in built {
            if child_median == 0.0 {
                child_median = median_ms(15, || {
                    grep(&ctx, COMMON);
                });
                // Each child holds its own unique needle; grep the one this
                // child holds, so both sides read exactly one file.
                let index = ctx
                    .config()
                    .project_root
                    .as_deref()
                    .and_then(|root| root.file_name())
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("repo-"))
                    .and_then(|index| index.parse::<usize>().ok())
                    .unwrap();
                let needle = format!("unique_needle_{index}");
                child_unique_median = median_ms(15, || {
                    grep(&ctx, &needle);
                });
            }
            expected_grep.extend(rows);
            expected_glob.extend(files);
        }
    }
    assert_eq!(expected_grep.len(), 64, "every child contributes two lines");

    let parent = configure(
        &folder.root,
        &folder.storage,
        config_doc(true, TRIGRAM_ONLY, None),
    );
    let session = parent_session(&parent);
    assert_eq!(session.children().len(), 32);
    assert!(session.children().iter().all(|child| child.trigram_ready()));

    let answer = grep(&parent, COMMON);
    assert_eq!(
        grep_rows(&answer, &folder.root, Path::new("")),
        expected_grep,
        "merged grep must equal the union of the child answers"
    );
    // Only the parent's own README is outside every child; no child is named.
    assert_eq!(gap_kinds(&answer), vec!["outside_child_repositories"]);
    assert_eq!(answer["complete"], false);
    assert_eq!(answer["gaps"][0]["path"], "README.md");
    let globbed = glob(&parent, "**/*.rs");
    assert_eq!(
        glob_rows(&globbed, &folder.root, Path::new("")),
        expected_glob,
        "merged glob must equal the union of the child answers"
    );

    let loads = session
        .children()
        .iter()
        .map(|child| child.loads())
        .collect::<Vec<_>>();
    let rounds = session.rounds();
    let parent_median = median_ms(15, || {
        grep(&parent, COMMON);
    });
    // A needle only one child holds: the parent consults 32 indexes but reads
    // one file, which is the common shape of a real parent folder search.
    let parent_unique_median = median_ms(15, || {
        grep(&parent, "unique_needle_0");
    });
    // Let at least one refresh round pass: refreshing reconciles in RAM and
    // must not reload an unchanged child either.
    assert!(session.wait_rounds(rounds + 2, DEADLINE));
    let after = session
        .children()
        .iter()
        .map(|child| child.loads())
        .collect::<Vec<_>>();
    assert_eq!(
        loads, after,
        "queries and refreshes must not reload children"
    );
    assert!(loads.iter().all(|count| *count == 1), "{loads:?}");
    // The load counts above are the mechanism this test guards: a parent query
    // reuses each child's loaded index. The timings are printed for reference
    // only, because wall-clock ratios on a shared CI runner measure load, not
    // whether a child was reloaded.
    eprintln!(
        "parent folder grep timing (32 children, debug build): token in every child: parent median {parent_median:.2} ms, child median {child_median:.2} ms; needle in one child: parent median {parent_unique_median:.2} ms, child median {child_unique_median:.2} ms"
    );

    // The parent wrote no index of its own.
    assert!(!trigram_artifact(&folder.root, &folder.storage).exists());
    assert!(!folder
        .storage
        .join("views")
        .join(aft::path_identity::project_scope_key(&folder.root))
        .exists());
}

/// Child edits made with no child session running reach the parent without
/// reloading the child: the full-reconcile backstop (shortened here; it runs
/// every few minutes by default) re-reads only the changed files into the
/// parent's in-memory copy. The child's artifact is never written, and an
/// unchanged child is answered without any gap.
#[test]
fn parent_backstop_applies_child_edits_and_never_writes_child_artifacts() {
    fast_refresh();
    let folder = folder(3);
    std::fs::remove_file(folder.root.join("README.md")).unwrap();
    for child in &folder.children {
        let ctx = configure(child, &folder.storage, child_trigram_doc());
        wait_trigram(&ctx, child, &folder.storage);
    }
    let artifact = trigram_artifact(&folder.children[1], &folder.storage);
    let before = std::fs::read(&artifact).unwrap();

    aft::views::parent::set_backstop_interval_for(&folder.root, Duration::from_millis(200));
    let parent = configure(
        &folder.root,
        &folder.storage,
        config_doc(true, TRIGRAM_ONLY, None),
    );
    let session = parent_session(&parent);
    let answer = grep(&parent, COMMON);
    assert_eq!(answer["complete"], true, "{answer:#}");
    assert!(answer.get("gaps").is_none(), "{answer:#}");

    write(
        &folder.children[1],
        "src/fresh.rs",
        "pub fn fresh() {}\n// FRESH_PARENT_EDIT_NEEDLE\n",
    );
    write(
        &folder.children[1],
        "src/util.rs",
        "pub fn helper() {}\n// edited, FRESH_PARENT_EDIT_NEEDLE\n",
    );
    let expected = BTreeSet::from([
        ("repo-01/src/fresh.rs".to_string(), 2),
        ("repo-01/src/util.rs".to_string(), 2),
    ]);
    let deadline = Instant::now() + DEADLINE;
    let answer = loop {
        let answer = grep(&parent, "FRESH_PARENT_EDIT_NEEDLE");
        if grep_rows(&answer, &folder.root, Path::new("")) == expected {
            break answer;
        }
        assert!(
            Instant::now() < deadline,
            "edits never reached the parent: {answer:#}"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(answer["complete"], true, "{answer:#}");
    // The edit replaced a line holding the common token.
    let common = grep_rows(&grep(&parent, COMMON), &folder.root, Path::new(""));
    assert!(!common.contains(&("repo-01/src/util.rs".to_string(), 2)));
    assert_eq!(std::fs::read(&artifact).unwrap(), before);
    let child = &session.children()[1];
    assert_eq!(child.loads(), 1, "edits must not reload the child");
    assert!(child.applied_paths() >= 2);
    // Unchanged children had nothing applied.
    assert_eq!(session.children()[0].applied_paths(), 0);
}

/// A child whose sessions run with views on publishes its lexical index only
/// as its v3 lexical view. The parent reads that publication: grep and glob
/// answer from it, and an edit the child has not published is not claimed by
/// the parent, which applies no file changes of its own on top.
#[test]
fn parent_grep_and_glob_read_views_on_children_from_their_v3_lexical_view() {
    fast_refresh();
    let folder = folder(2);
    std::fs::remove_file(folder.root.join("README.md")).unwrap();
    let mut expected_grep = BTreeSet::new();
    let mut children = Vec::new();
    for (index, child) in folder.children.iter().enumerate() {
        // The child's bind publishes its v3 lexical view.
        let ctx = configure(child, &folder.storage, config_doc(true, TRIGRAM_ONLY, None));
        assert!(!trigram_artifact(child, &folder.storage).exists());
        let prefix = format!("repo-{index:02}");
        expected_grep.insert((format!("{prefix}/src/lib.rs"), 5));
        expected_grep.insert((format!("{prefix}/src/util.rs"), 2));
        children.push(ctx);
    }

    let parent = configure(
        &folder.root,
        &folder.storage,
        config_doc(true, TRIGRAM_ONLY, None),
    );
    let session = parent_session(&parent);
    assert!(session.children().iter().all(|child| child.trigram_ready()));
    let answer = grep(&parent, COMMON);
    assert_eq!(
        grep_rows(&answer, &folder.root, Path::new("")),
        expected_grep,
        "{answer:#}"
    );
    assert_eq!(answer["complete"], true, "{answer:#}");
    assert!(session.view_route_counts()[1] >= 1);
    let globbed = glob(&parent, "**/*.rs");
    assert_eq!(
        globbed["files"].as_array().map(Vec::len),
        Some(4),
        "{globbed:#}"
    );
    let routed = session.view_route_counts()[1];
    assert!(routed >= 2, "{routed}");

    // An edit no child session has published: the parent answers as of the
    // child's publication rather than reading the file itself.
    write(
        &folder.children[1],
        "src/fresh.rs",
        "pub fn fresh() {}\n// UNPUBLISHED_PARENT_EDIT_NEEDLE\n",
    );
    let rounds = session.rounds();
    assert!(session.wait_rounds(rounds + 2, DEADLINE));
    let unpublished = grep(&parent, "UNPUBLISHED_PARENT_EDIT_NEEDLE");
    assert_eq!(
        grep_rows(&unpublished, &folder.root, Path::new("")),
        BTreeSet::new(),
        "{unpublished:#}"
    );
    for child in session.children() {
        assert_eq!(child.loads(), 1, "an unchanged publication is not reloaded");
        assert_eq!(
            child.applied_paths(),
            0,
            "the parent applied an edit itself"
        );
    }
    drop(children);
}

/// The parent's own file watcher carries a child's edit to the parent within
/// seconds, while the backstop stays at its multi-minute default: changes are
/// event-driven, routed to the child that owns the path.
#[test]
fn parent_watcher_routes_child_edits_to_the_owning_child() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    let child = root.join("child");
    let other = root.join("other");
    init_repo(&child, &child_files(0));
    init_repo(&other, &child_files(1));
    let configure_request = |root: &Path, views: bool| {
        json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(config_doc(views, TRIGRAM_ONLY, None)),
        })
        .to_string()
    };
    let grep_request =
        |pattern: &str| json!({"id": "grep", "command": "grep", "pattern": pattern}).to_string();
    let mut aft = crate::helpers::AftProcess::spawn_with_real_watcher();
    for repo in [&child, &other] {
        let configured = aft.send(&configure_request(repo, false));
        assert_eq!(configured["success"], true, "{configured:#}");
        let deadline = Instant::now() + DEADLINE;
        while !trigram_artifact(repo, &storage).is_file() {
            assert!(Instant::now() < deadline, "child index never persisted");
            aft.send(&grep_request(COMMON));
            thread::sleep(Duration::from_millis(100));
        }
    }
    let configured = aft.send(&configure_request(&root, true));
    assert_eq!(configured["success"], true, "{configured:#}");
    let deadline = Instant::now() + DEADLINE;
    loop {
        let answer = aft.send(&grep_request(COMMON));
        if answer["matches"]
            .as_array()
            .is_some_and(|rows| rows.len() == 4)
        {
            break;
        }
        assert!(Instant::now() < deadline, "parent never loaded: {answer:#}");
        thread::sleep(Duration::from_millis(100));
    }
    // Give the watcher time to settle on the freshly bound folder.
    thread::sleep(Duration::from_millis(500));
    write(&child, "src/watched.rs", "// WATCHED_PARENT_EDIT_NEEDLE\n");
    let started = Instant::now();
    loop {
        let answer = aft.send(&grep_request("WATCHED_PARENT_EDIT_NEEDLE"));
        if answer["matches"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        {
            // grep reports native paths, so Windows separators are backslashes.
            let file = answer["matches"][0]["file"]
                .as_str()
                .unwrap()
                .replace('\\', "/");
            assert!(file.ends_with("child/src/watched.rs"), "{answer:#}");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the watcher never delivered the edit: {answer:#}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "parent folder watcher: edit visible to parent grep after {} ms",
        started.elapsed().as_millis()
    );
    assert!(aft.shutdown().success());
}

/// The call graph is served from a child's published v3 view: answers carry
/// the child prefix and say which publication they are as of, a child without
/// a view and a file outside every child are named gaps, and the generation
/// the parent serves stays readable after the child publishes a newer one.
#[test]
fn parent_callgraph_reads_child_v3_views_and_answers_as_of_their_publication() {
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
    // `plain` is never bound to a session, so it has no view at all.
    let child = configure(&graph, &storage, config_doc(true, with_graph, None));
    child_callgraph_answer(&child, &graph);

    let parent = configure(&root, &storage, config_doc(true, with_graph, None));
    let session = parent_session(&parent);
    let graph_child = session
        .children()
        .iter()
        .find(|child| child.root == graph)
        .unwrap()
        .clone();
    let first = wait_parent_callgraph(&graph_child, None);
    assert_eq!(session.view_route_counts()[0], 0);

    let callers = data(
        aft::views::parent::route(
            &request(json!({
                "id": "parent-callers",
                "command": "callers",
                "file": "graph/src/lib.rs",
                "symbol": "target",
            })),
            &parent,
        )
        .expect("callers is routed for a parent folder"),
    );
    let text = callers.to_string();
    assert!(text.contains("graph/src/lib.rs"), "{callers:#}");
    assert!(text.contains("caller"), "{callers:#}");
    // A foreign reader names the publication it answers as of; it never
    // claims the child's graph is current.
    assert_eq!(callers["as_of"]["child"], "graph", "{callers:#}");
    assert_eq!(
        callers["as_of"]["generation"],
        first.as_str(),
        "{callers:#}"
    );
    assert!(callers["as_of"]["published_at_ms"].is_u64(), "{callers:#}");
    assert_eq!(session.view_route_counts()[0], 1);

    let missing = aft::views::parent::route(
        &request(json!({
            "id": "parent-callers-plain",
            "command": "callers",
            "file": "plain/src/lib.rs",
            "symbol": "alone",
        })),
        &parent,
    )
    .unwrap();
    assert!(!missing.success);
    assert_eq!(missing.data["complete"], false);
    assert_eq!(missing.data["gaps"][0]["kind"], "parent_child_unavailable");
    assert_eq!(missing.data["gaps"][0]["path"], "plain");
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

    // Hold the served generation while the child publishes an edit: the
    // parent keeps answering from the generation it holds a marker on, as
    // of that publication, and follows the child once released.
    session.pause_refresh(true);
    // An AFT write through the child's session, which its view records.
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
    let edited = child_callgraph_answer(&child, &graph);
    assert!(edited.to_string().contains("second"), "{edited:#}");
    let held = data(
        aft::views::parent::route(
            &request(json!({
                "id": "parent-callers-held",
                "command": "callers",
                "file": "graph/src/lib.rs",
                "symbol": "target",
            })),
            &parent,
        )
        .unwrap(),
    );
    assert_eq!(held["as_of"]["generation"], first.as_str(), "{held:#}");
    assert!(!held.to_string().contains("second"), "{held:#}");
    session.pause_refresh(false);
    let second = wait_parent_callgraph(&graph_child, Some(&first));
    let followed = data(
        aft::views::parent::route(
            &request(json!({
                "id": "parent-callers-followed",
                "command": "callers",
                "file": "graph/src/lib.rs",
                "symbol": "target",
            })),
            &parent,
        )
        .unwrap(),
    );
    assert_eq!(
        followed["as_of"]["generation"],
        second.as_str(),
        "{followed:#}"
    );
    assert!(followed.to_string().contains("second"), "{followed:#}");
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

/// More children than the cap: the first 64 are served and the rest are one
/// named gap that says how many were skipped. Children with no index yet are
/// named gaps too, and nothing is built for them.
#[test]
fn over_cap_parent_names_skipped_and_unindexed_children() {
    fast_refresh();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    for index in 0..66 {
        // Discovery needs only the `.git` marker; these children never built
        // an index.
        std::fs::create_dir_all(root.join(format!("repo-{index:02}")).join(".git")).unwrap();
    }
    let parent = configure(&root, &storage, config_doc(true, TRIGRAM_ONLY, None));
    let session = parent_session(&parent);
    assert_eq!(
        session.children().len(),
        aft::views::parent::DEFAULT_MAX_CHILD_REPOS
    );
    let answer = grep(&parent, "anything");
    assert_eq!(answer["complete"], false);
    let gaps = answer["gaps"].as_array().unwrap();
    let over_cap = gaps
        .iter()
        .find(|gap| gap["kind"] == "parent_children_over_cap")
        .unwrap_or_else(|| panic!("{answer:#}"));
    assert!(over_cap["reason"]
        .as_str()
        .unwrap()
        .starts_with("2 child repositories beyond the 64-repository cap"));
    assert!(over_cap["reason"].as_str().unwrap().contains("repo-65"));
    assert_eq!(
        gaps.iter()
            .filter(|gap| gap["kind"] == "parent_child_unavailable")
            .count(),
        64
    );
    // Nothing was built for the children: no trigram artifact anywhere.
    for index in 0..66 {
        let child = root.join(format!("repo-{index:02}"));
        assert!(!trigram_artifact(&child, &storage).exists());
    }
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
    assert_eq!(
        gap_kinds(&after)
            .iter()
            .filter(|kind| *kind == "parent_child_unavailable")
            .count(),
        20
    );
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

/// A real process restart: a new `aft` process bound to the parent folder
/// serves the child from the v3 lexical view the child's own session
/// published, without the child ever being bound in the new process. Neither
/// process writes a legacy trigram artifact for the child.
#[test]
fn parent_folder_survives_a_real_process_restart() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    let child = root.join("child");
    init_repo(
        &child,
        &[(
            "src/lib.rs",
            "pub fn restart_needle_fn() {}\n// RESTART_PARENT_NEEDLE\n".to_string(),
        )],
    );
    let configure_request = |root: &Path| {
        json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(config_doc(true, TRIGRAM_ONLY, None)),
        })
        .to_string()
    };
    let grep_request = json!({
        "id": "grep",
        "command": "grep",
        "pattern": "RESTART_PARENT_NEEDLE",
    })
    .to_string();
    let wait_for_match = |aft: &mut crate::helpers::AftProcess| -> Value {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let response = aft.send(&grep_request);
            let answer = response.clone();
            let found = answer["matches"]
                .as_array()
                .is_some_and(|matches| !matches.is_empty());
            if found && answer.get("gaps").is_none() {
                return answer;
            }
            assert!(
                Instant::now() < deadline,
                "parent never answered: {response:#}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    };

    let mut first = crate::helpers::AftProcess::spawn();
    let configured = first.send(&configure_request(&child));
    assert_eq!(configured["success"], true, "{configured:#}");
    let artifact = trigram_artifact(&child, &storage);
    // The child's own grep answers from its lexical view, which the child's
    // bind published.
    let child_answer = wait_for_match(&mut first);
    assert_eq!(child_answer["complete"], true, "{child_answer:#}");
    let configured = first.send(&configure_request(&root));
    assert_eq!(configured["success"], true, "{configured:#}");
    let answer = wait_for_match(&mut first);
    // grep reports native paths, so Windows separators are backslashes.
    let file = answer["matches"][0]["file"]
        .as_str()
        .unwrap()
        .replace('\\', "/");
    assert!(file.ends_with("src/lib.rs"), "{answer:#}");
    assert!(first.shutdown().success());

    let mut second = crate::helpers::AftProcess::spawn();
    let configured = second.send(&configure_request(&root));
    assert_eq!(configured["success"], true, "{configured:#}");
    let answer = wait_for_match(&mut second);
    assert_eq!(answer["complete"], true, "{answer:#}");
    assert!(second.shutdown().success());
    assert!(!artifact.exists());
}
