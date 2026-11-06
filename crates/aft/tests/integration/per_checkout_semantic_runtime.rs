//! Semantic search on the data layer through the real configure and search
//! entry points: a root's checkout fills its meaning plane in the
//! background, and `aft_search` scores it.
//!
//! The embedding backend is a local HTTP mock that counts every text it is
//! asked to embed, so the tests measure real model traffic: identical content
//! in two worktrees of one repository is embedded once, a later session on
//! the same worktrees embeds nothing, two processes never embed one chunk
//! twice, and the data layer's answers rank exactly like today's index on the
//! same content.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

const DEADLINE: Duration = Duration::from_secs(90);
const FINGERPRINT_PROBE: &str = "semantic index fingerprint probe";
const COMPLETION_QUERY: &str = "single-flight-readiness-probe";

/// Deterministic embeddings over HTTP, counting texts other than the known
/// fingerprint and readiness-query probes (neither is chunk-vector work).
struct MockEmbedder {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    texts: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbedder {
    fn start() -> Self {
        Self::with_gate(None)
    }

    fn with_gate(gate: Option<Arc<EmbeddingGate>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let texts = Arc::new(AtomicUsize::new(0));
        let (thread_running, thread_texts) = (Arc::clone(&running), Arc::clone(&texts));
        let handle = thread::spawn(move || {
            while thread_running.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let texts = Arc::clone(&thread_texts);
                let gate = gate.clone();
                thread::spawn(move || {
                    let _ = serve(&mut stream, &texts, gate.as_deref());
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            texts,
            handle: Some(handle),
        }
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct EmbeddingGate {
    state: Mutex<(Vec<Vec<String>>, bool)>,
    changed: Condvar,
}

impl EmbeddingGate {
    fn arrive(&self, inputs: &[String]) {
        if !inputs
            .iter()
            .any(|text| text.contains("concurrent_paid_chunk"))
        {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.0.push(inputs.to_vec());
        self.changed.notify_all();
        let deadline = Instant::now() + DEADLINE;
        while !state.1 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "embedding response was not released");
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }

    fn wait_for_first(&self) -> Vec<Vec<String>> {
        let mut state = self.state.lock().unwrap();
        let deadline = Instant::now() + DEADLINE;
        while state.0.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "only {} processes reached the model",
                state.0.len()
            );
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
        state.0.clone()
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

struct ReleaseEmbeddingGate(Arc<EmbeddingGate>);

impl Drop for ReleaseEmbeddingGate {
    fn drop(&mut self) {
        self.0.release();
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

fn serve(
    stream: &mut TcpStream,
    texts: &AtomicUsize,
    gate: Option<&EmbeddingGate>,
) -> std::io::Result<()> {
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
    texts.fetch_add(
        inputs
            .iter()
            .filter(|input| *input != FINGERPRINT_PROBE && !input.contains(COMPLETION_QUERY))
            .count(),
        Ordering::SeqCst,
    );
    if let Some(gate) = gate {
        gate.arrive(&inputs);
    }
    // Vectors depend on the model too, so vectors of one model never rank
    // like another's: a test can tell which model's vectors were scored.
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            json!({ "embedding": vector(&format!("{model}\u{0}{input}")), "index": index })
        })
        .collect::<Vec<_>>();
    let body = json!({ "data": data }).to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

const FILES: &[(&str, &str)] = &[
    (
        "src/cache.rs",
        "pub struct LruCache {\n    capacity: usize,\n}\n\nimpl LruCache {\n    pub fn evict_oldest(&mut self) -> Option<String> {\n        None\n    }\n}\n",
    ),
    (
        "src/parse.rs",
        "pub fn parse_config_file(path: &str) -> Option<String> {\n    std::fs::read_to_string(path).ok()\n}\n\nfn trim_comment(line: &str) -> &str {\n    line.split('#').next().unwrap_or(line)\n}\n",
    ),
    (
        "src/retry.rs",
        "pub fn retry_with_backoff(attempts: u32) -> u64 {\n    2u64.pow(attempts)\n}\n",
    ),
    (
        "src/lib.rs",
        "pub mod cache;\npub mod parse;\npub mod retry;\n",
    ),
];

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

/// A repository with one commit and a second worktree of it.
struct Repository {
    _temp: tempfile::TempDir,
    main: PathBuf,
    linked: PathBuf,
}

fn repository() -> Repository {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let main = base.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "views@example.test"]);
    git(&main, &["config", "user.name", "Views Test"]);
    for (path, text) in FILES {
        write(&main, path, text);
    }
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "initial"]);
    let linked = base.join("linked");
    // `canonicalize` gives a verbatim `\\?\C:\...` path on Windows, which git
    // rejects when creating the worktree, so git gets the plain form. The
    // canonical form stays in `linked` for AFT, which compares it with the
    // paths it resolves itself.
    let linked_arg = linked.to_str().unwrap();
    let linked_arg = linked_arg.strip_prefix(r"\\?\").unwrap_or(linked_arg);
    git(&main, &["worktree", "add", "-q", linked_arg, "HEAD"]);
    Repository {
        _temp: temp,
        main,
        linked,
    }
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).unwrap()
}

const MODEL: &str = "views-semantic-mock";

fn configure(root: &Path, storage: &Path, server: &MockEmbedder, views: bool) -> Arc<AppContext> {
    let ctx = Arc::new(
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        )
        .with_test_checkpoint_store(),
    );
    send_configure(&ctx, root, storage, server, views, MODEL);
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    ctx
}

/// Sends one configure request without running its deferred maintenance.
fn send_configure(
    ctx: &AppContext,
    root: &Path,
    storage: &Path,
    server: &MockEmbedder,
    views: bool,
    model: &str,
) {
    let configured = aft::commands::configure::handle_configure(
        &request(configure_payload(
            root, storage, server, views, model, 5_000,
        )),
        ctx,
    );
    assert!(configured.success, "configure failed: {configured:?}");
    let resolved = ctx.config().storage_dir.clone().expect("resolved storage");
    eprintln!(
        "semantic configure: root={} requested={} resolved={}",
        root.display(),
        storage.display(),
        resolved.display()
    );
    assert_eq!(
        crate::helpers::canonicalize_like_product(&resolved),
        crate::helpers::canonicalize_like_product(storage),
        "fixture storage was overridden"
    );
}

fn configure_payload(
    root: &Path,
    storage: &Path,
    server: &MockEmbedder,
    views: bool,
    model: &str,
    timeout_ms: u64,
) -> Value {
    json!({
        "id": "configure-views-semantic",
        "command": "configure",
        "harness": "opencode",
        "project_root": root,
        "storage_dir": storage,
        "config": crate::helpers::user_config(json!({
            "indexes": { "trigram": true, "semantic": true, "callgraph": false },
            "views": { "enabled": views },
            "semantic": {
                "backend": "openai_compatible",
                "model": model,
                "base_url": server.base_url,
                "timeout_ms": timeout_ms,
                "max_batch_size": 64,
                "max_files": 2_000
            }
        }))
    })
}

fn drain(ctx: &AppContext) {
    aft::runtime_drain::drain_watcher_events(ctx);
    aft::runtime_drain::drain_search_index_events(ctx);
    aft::runtime_drain::drain_semantic_index_events(ctx);
    aft::runtime_drain::drain_semantic_refresh_events(ctx);
}

fn search(ctx: &AppContext, query: &str) -> Value {
    let started = Instant::now();
    let answer = serde_json::to_value(aft::commands::semantic_search::handle_semantic_search(
        &request(json!({ "id": "views-semantic-search", "command": "search", "query": query })),
        ctx,
    ))
    .unwrap();
    eprintln!(
        "semantic search query={query:?} elapsed={:?} status={} complete={}\n{}",
        started.elapsed(),
        answer["status"],
        answer["complete"],
        answer["text"]
    );
    if answer["complete"] != true {
        eprintln!("incomplete semantic search reply: {answer:#}");
    }
    answer
}

/// The ranked rows of a search answer, relative to `root`.
fn ranked(root: &Path, answer: &Value) -> Vec<Value> {
    answer["results"]
        .as_array()
        .unwrap_or_else(|| panic!("search failed: {answer:#}"))
        .iter()
        .map(|row| {
            let file = row["file"].as_str().unwrap_or_default();
            let file = Path::new(file)
                .strip_prefix(root)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| file.to_owned());
            json!({ "file": file, "name": row["name"], "line": row["line"], "score": row["score"] })
        })
        .collect()
}

/// The number of texts a legacy build embeds for `root`: its unique chunks.
fn unique_chunks(root: &Path) -> usize {
    let files = aft::views::first_load::MembershipWalker::files(
        &aft::views::first_load::ConfiguredMembershipWalker,
        root,
    )
    .unwrap();
    let mut texts = 0;
    aft::semantic_index::SemanticIndex::build(
        root,
        &files,
        &mut |batch: Vec<String>| {
            texts += batch.len();
            Ok(batch.iter().map(|text| vector(text)).collect())
        },
        64,
    )
    .unwrap();
    texts
}

/// The same three scenarios on the data layer (`AFT_DATA_LAYER=1`), whose
/// vector plane is filled per repository store: while one process's fill
/// round embeds a chunk, a fill of the same repository in another process
/// waits for it and then finds the vector stored, so the chunk is paid for
/// once. The waiting process reports `stage: waiting_for_another_fill` and
/// `waiting_for_another_fill: true` on status where the views lane reported
/// `pending_reason` and `coordination_error`.
#[test]
fn data_layer_two_processes_single_flight_checkout_chunk() {
    data_layer_two_process_single_flight(false, false);
}

#[test]
fn data_layer_two_processes_single_flight_separate_base_scopes() {
    data_layer_two_process_single_flight(true, true);
}

#[test]
fn data_layer_two_processes_single_flight_inherited_and_checkout_chunk() {
    data_layer_two_process_single_flight(true, false);
}

/// A standalone AFT process running the data layer over `storage`.
fn data_layer_process(storage: &Path) -> crate::helpers::AftProcess {
    crate::helpers::AftProcess::spawn_with_semantic_env(&[
        ("AFT_STORAGE_DIR", storage.as_os_str()),
        ("AFT_DATA_LAYER", std::ffi::OsStr::new("1")),
    ])
}

/// Configures `process` on `root` with the mock embedder, views off.
fn configure_data_layer(
    process: &mut crate::helpers::AftProcess,
    root: &Path,
    storage: &Path,
    server: &MockEmbedder,
) {
    let configured =
        process.send(&configure_payload(root, storage, server, false, MODEL, 120_000).to_string());
    assert_eq!(configured["success"], true, "{configured:?}");
}

/// Waits until the process's data layer has published its text plane and
/// filled its meaning plane, and returns that status.
fn wait_data_layer_filled(process: &mut crate::helpers::AftProcess) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let status = process.send(r#"{"id":"data-layer-ready","command":"status"}"#);
        if status["search_index"]["status"] == "ready"
            && status["semantic_index"]["status"] == "ready"
        {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the data layer never filled: {status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn data_layer_search(process: &mut crate::helpers::AftProcess, id: &str) -> Value {
    process.send(
        &json!({"id": id, "command": "semantic_search", "query": COMPLETION_QUERY, "top_k": 100})
            .to_string(),
    )
}

fn data_layer_two_process_single_flight(commit_main: bool, commit_linked: bool) {
    let _settings = crate::test_helpers::without_storage_override();
    let gate = Arc::new(EmbeddingGate::default());
    let server = MockEmbedder::with_gate(Some(Arc::clone(&gate)));
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let chunks = unique_chunks(&repo.main);
    {
        let mut warm = data_layer_process(storage.path());
        configure_data_layer(&mut warm, &repo.main, storage.path(), &server);
        wait_data_layer_filled(&mut warm);
        assert_eq!(
            server.texts(),
            chunks,
            "the data layer embeds each chunk once and builds no second index"
        );
    }
    // The same new function in both worktrees, uncommitted unless a
    // scenario commits it in one or both.
    for root in [&repo.main, &repo.linked] {
        write(
            root,
            "src/concurrent.rs",
            "pub fn concurrent_paid_chunk() -> u64 { 424242 }\n",
        );
    }
    if commit_main {
        git(&repo.main, &["add", "src/concurrent.rs"]);
        git(&repo.main, &["commit", "-m", "new inherited chunk"]);
    }
    if commit_linked {
        write(&repo.linked, ".gitignore", "# distinct base tree\n");
        git(&repo.linked, &["add", "src/concurrent.rs", ".gitignore"]);
        git(
            &repo.linked,
            &["commit", "-m", "different base with shared chunk"],
        );
    }
    let mut main = data_layer_process(storage.path());
    let mut linked = data_layer_process(storage.path());
    let _release = ReleaseEmbeddingGate(Arc::clone(&gate));
    eprintln!(
        "two-process data-layer fill: main_pid={} linked_pid={} storage={}",
        main.pid(),
        linked.pid(),
        storage.path().display()
    );
    configure_data_layer(&mut main, &repo.main, storage.path(), &server);
    let requests = gate.wait_for_first();
    assert_eq!(requests[0].len(), 1, "one new function is one chunk");
    configure_data_layer(&mut linked, &repo.linked, storage.path(), &server);
    let deadline = Instant::now() + DEADLINE;
    let waiting = loop {
        let status = linked.send(r#"{"id":"contention","command":"status"}"#);
        assert_eq!(
            server.texts() - chunks,
            1,
            "the second process embedded the chunk the first is embedding"
        );
        if status["semantic_index"]["waiting_for_another_fill"] == true {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the second process never waited for the first one's fill: {status}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    eprintln!("waiting process status: {}", waiting["semantic_index"]);
    assert_eq!(
        waiting["semantic_index"]["stage"], "waiting_for_another_fill",
        "{waiting}"
    );
    assert_eq!(waiting["semantic_index"]["status"], "building", "{waiting}");
    assert_eq!(gate.state.lock().unwrap().0.len(), 1);
    // Waiting is not an outage: the stored vectors still serve, and the
    // new file is named as waiting.
    let pending = data_layer_search(&mut linked, "pending-query");
    assert_eq!(pending["complete"], false, "{pending}");
    assert!(
        pending["results"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "stored vectors stopped serving while the fill waited: {pending}"
    );
    assert!(
        pending["text"]
            .as_str()
            .is_some_and(|text| text.contains("src/concurrent.rs")),
        "{pending}"
    );
    gate.release();
    for process in [&mut main, &mut linked] {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let answer = data_layer_search(process, "ready-query");
            if answer["complete"] == true {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the meaning plane never filled: {answer}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(
        server.texts() - chunks,
        1,
        "the second process embedded the chunk after the first published it"
    );
    drop(main);
    drop(linked);

    // Later sessions on both worktrees embed nothing and rank alike.
    let mut answers = Vec::new();
    for root in [&repo.main, &repo.linked] {
        let mut reopened = data_layer_process(storage.path());
        configure_data_layer(&mut reopened, root, storage.path(), &server);
        wait_data_layer_filled(&mut reopened);
        // A reply can still name watcher changes the process has not applied
        // yet (the `watcher_pending` gap), which also makes it incomplete;
        // ask again until it is complete.
        let deadline = Instant::now() + DEADLINE;
        let answer = loop {
            let answer = data_layer_search(&mut reopened, "reopened-query");
            if answer["complete"] == true {
                break answer;
            }
            assert!(
                Instant::now() < deadline,
                "a reopened session never answered completely: {answer}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        answers.push(ranked(root, &answer));
    }
    assert!(!answers[0].is_empty());
    assert_eq!(answers[0], answers[1]);
    assert_eq!(
        server.texts() - chunks,
        1,
        "reopened sessions must reuse stored vectors"
    );
}

/// Restores an environment variable when dropped.
struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: changed only while `watcher_serial_lock` is held.
        unsafe {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

/// `ctx`'s status, after draining what its background work sent.
fn status(ctx: &AppContext) -> Value {
    drain(ctx);
    ctx.build_status_snapshot()
}

/// Waits until `ctx`'s data layer serves text and meaning search: its
/// status reports the text index and the meaning plane ready (every file
/// filled). Status never embeds, so the mock counts fill texts only.
fn wait_data_layer_ready(ctx: &AppContext) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let status = status(ctx);
        if status["search_index"]["status"] == "ready"
            && status["semantic_index"]["status"] == "ready"
        {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the data layer never filled: search_index={} semantic_index={}",
            status["search_index"],
            status["semantic_index"]
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// A prose query on a filled checkout answers complete, with the matching
/// symbol's preview, and the recall audit's semantic diagnostics.
#[test]
fn prose_watchdog_query_reports_coverage_and_keeps_symbol_previews() {
    let _settings = crate::test_helpers::without_storage_override();
    let _serial = crate::helpers::watcher_serial_lock();
    let _audit = EnvGuard(
        "AFT_SEARCH_RECALL_AUDIT",
        std::env::var_os("AFT_SEARCH_RECALL_AUDIT"),
    );
    // SAFETY: environment changes in this module hold watcher_serial_lock.
    unsafe { std::env::set_var("AFT_SEARCH_RECALL_AUDIT", "1") };
    let server = MockEmbedder::start();
    let repo = repository();
    write(&repo.main, "src/watchdog.rs",
        "// The bash watchdog stops background tasks when their time budget expires.\n\npub fn kill_expired_task() {\n    terminate_child();\n}\n");
    let storage = tempfile::tempdir().unwrap();
    let ctx = configure(&repo.main, storage.path(), &server, false);
    let ready = wait_data_layer_ready(&ctx);
    assert!(
        ready["semantic_index"]["chunks_embedded"].as_u64() > Some(0),
        "{}",
        ready["semantic_index"]
    );
    let query = "how does the bash watchdog kill a background task that exceeded its timeout";
    let answer = search(&ctx, query);
    let audit = &answer["recall_audit"]["semantic_preview"];
    assert!(
        audit["metadata"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "missing semantic metadata diagnostics\n{answer:#}"
    );
    assert_eq!(
        audit["ranked_page"].as_array().map(Vec::len),
        answer["results"].as_array().map(Vec::len),
        "missing ranked-path diagnostics\n{answer:#}"
    );
    assert_eq!(answer["complete"], true, "{answer:#}");
    let text = answer["text"].as_str().unwrap();
    assert!(
        text.contains("kill_expired_task [function] lines 3-5"),
        "{text}"
    );
    assert!(text.contains("terminate_child();"), "{text}");
}

/// Two worktrees of one repository, sharing one storage root, embed each
/// chunk once, and a later session on them embeds nothing. Today's index is
/// not built beside the plane.
#[test]
fn data_layer_worktrees_and_sessions_embed_each_chunk_once() {
    let _settings = crate::test_helpers::without_storage_override();
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let chunks = unique_chunks(&repo.main);
    assert!(chunks > 0);
    {
        let main = configure(&repo.main, storage.path(), &server, false);
        wait_data_layer_ready(&main);
        let linked = configure(&repo.linked, storage.path(), &server, false);
        wait_data_layer_ready(&linked);
        assert_eq!(
            server.texts(),
            chunks,
            "two worktrees with identical content must embed each chunk once"
        );
        assert!(
            main.semantic_index().read().unwrap().is_none(),
            "the data layer serves meaning search: today's index must not be built"
        );
    }
    let embedded = server.texts();
    let main = configure(&repo.main, storage.path(), &server, false);
    wait_data_layer_ready(&main);
    let linked = configure(&repo.linked, storage.path(), &server, false);
    wait_data_layer_ready(&linked);
    assert_eq!(
        server.texts(),
        embedded,
        "a later session re-embedded content its repository already holds"
    );
}

/// Reconfiguring a root with another embedding model fills its plane again
/// under that model: every chunk is embedded once more, and the answers rank
/// exactly like a root configured with the new model from the start (the
/// mock's vectors depend on the model, so any vector of the old model would
/// change them).
#[test]
fn data_layer_model_change_fills_again_with_the_new_model() {
    let _settings = crate::test_helpers::without_storage_override();
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let chunks = unique_chunks(&repo.main);
    let ctx = configure(&repo.main, storage.path(), &server, false);
    wait_data_layer_ready(&ctx);
    assert_eq!(server.texts(), chunks);

    send_configure(
        &ctx,
        &repo.main,
        storage.path(),
        &server,
        false,
        "views-semantic-mock-2",
    );
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let deadline = Instant::now() + DEADLINE;
    while server.texts() < 2 * chunks {
        assert!(
            Instant::now() < deadline,
            "the new model never embedded the checkout: {} texts",
            server.texts()
        );
        thread::sleep(Duration::from_millis(20));
    }
    wait_data_layer_ready(&ctx);
    assert_eq!(
        server.texts(),
        2 * chunks,
        "the new model must embed every chunk once, reusing nothing of the old one"
    );

    let fresh_storage = tempfile::tempdir().unwrap();
    let fresh = Arc::new(
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        )
        .with_test_checkpoint_store(),
    );
    send_configure(
        &fresh,
        &repo.main,
        fresh_storage.path(),
        &server,
        false,
        "views-semantic-mock-2",
    );
    aft::runtime_drain::drain_deferred_configure_maintenance(&fresh);
    wait_data_layer_ready(&fresh);
    for query in [
        "evict the oldest cache entry",
        "exponential backoff between retries",
    ] {
        let expected = ranked(&repo.main, &search(&fresh, query));
        let actual = search(&ctx, query);
        assert_eq!(actual["complete"], true, "{actual:#}");
        assert!(!expected.is_empty(), "nothing found for {query:?}");
        assert_eq!(
            ranked(&repo.main, &actual),
            expected,
            "after a model change the answer ranks differently for {query:?}"
        );
    }
}

/// A standalone AFT process over `storage` on today's route
/// (`AFT_DATA_LAYER=0`), with semantic search left at the product default.
fn todays_route_process(storage: &Path) -> crate::helpers::AftProcess {
    crate::helpers::AftProcess::spawn_with_semantic_env(&[
        ("AFT_STORAGE_DIR", storage.as_os_str()),
        ("AFT_DATA_LAYER", std::ffi::OsStr::new("0")),
    ])
}

/// The data layer's answers rank exactly like today's index on the same
/// content, with the same embedder.
#[test]
fn data_layer_search_ranks_like_todays_index_on_the_same_content() {
    let _settings = crate::test_helpers::without_storage_override();
    let server = MockEmbedder::start();
    let repo = repository();
    let today_storage = tempfile::tempdir().unwrap();
    let data_storage = tempfile::tempdir().unwrap();
    let mut today = todays_route_process(today_storage.path());
    configure_data_layer(&mut today, &repo.main, today_storage.path(), &server);
    wait_data_layer_filled(&mut today);
    let mut data = data_layer_process(data_storage.path());
    configure_data_layer(&mut data, &repo.main, data_storage.path(), &server);
    wait_data_layer_filled(&mut data);

    for (index, query) in [
        "evict the oldest cache entry",
        "read a configuration file from disk",
        "exponential backoff between retries",
    ]
    .into_iter()
    .enumerate()
    {
        // A reply can still name watcher changes the process has not applied
        // yet (the `watcher_pending` gap), which also makes it incomplete;
        // ask again until it is complete.
        let ask = |process: &mut crate::helpers::AftProcess| {
            let deadline = Instant::now() + DEADLINE;
            loop {
                let answer = process.send(
                    &json!({"id": format!("rank-{index}"), "command": "semantic_search", "query": query, "top_k": 10})
                        .to_string(),
                );
                if answer["complete"] == true || Instant::now() >= deadline {
                    return answer;
                }
                thread::sleep(Duration::from_millis(20));
            }
        };
        let expected = ask(&mut today);
        let actual = ask(&mut data);
        assert_eq!(
            actual["complete"], true,
            "data layer incomplete: {actual:#}"
        );
        let expected_rows = ranked(&repo.main, &expected);
        assert!(
            !expected_rows.is_empty(),
            "today's index found nothing for {query}"
        );
        assert_eq!(
            ranked(&repo.main, &actual),
            expected_rows,
            "the data layer ranks differently for {query:?}"
        );
    }
}

/// Searches `ctx` (which starts a fill when files wait for vectors) until
/// the mock has embedded more than `before` texts and status reports the
/// meaning plane filled again.
fn wait_for_fill_after(ctx: &AppContext, server: &MockEmbedder, before: usize, query: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        search(ctx, query);
        let status = status(ctx);
        if server.texts() > before && status["semantic_index"]["status"] == "ready" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the edit was never embedded: {} texts, semantic_index={}",
            server.texts(),
            status["semantic_index"]
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// An edit through AFT's own write command and an edit made outside AFT,
/// delivered by the watcher, both reach the fill: their new chunks are
/// embedded and the plane is filled again.
#[test]
fn data_layer_aft_and_outside_edits_reach_the_fill() {
    let _settings = crate::test_helpers::without_storage_override();
    let _serial = crate::helpers::watcher_serial_lock();
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let ctx = configure(&repo.main, storage.path(), &server, false);
    wait_data_layer_ready(&ctx);

    // An edit through AFT's own write command.
    let before = server.texts();
    let written = aft::commands::write::handle_write(
        &request(json!({
            "id": "data-layer-semantic-write",
            "command": "write",
            "file": repo.main.join("src/retry.rs"),
            "content": "pub fn jittered_sleep_between_attempts(attempt: u32) -> u64 {\n    u64::from(attempt) * 10\n}\n"
        })),
        &ctx,
    );
    assert!(written.success, "write failed: {written:?}");
    wait_for_fill_after(&ctx, &server, before, "jittered sleep between attempts");

    // An edit made outside AFT, delivered by the watcher.
    let before = server.texts();
    let outside = repo.main.join("src/parse.rs");
    std::fs::write(
        &outside,
        "pub fn tokenize_shell_arguments(line: &str) -> Vec<String> {\n    line.split_whitespace().map(str::to_owned).collect()\n}\n",
    )
    .unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    tx.send(WatcherDispatchEvent::Paths(vec![outside])).unwrap();
    wait_for_fill_after(&ctx, &server, before, "tokenize shell arguments");
}
