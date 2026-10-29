//! Views-on semantic search through the real configure and search entry
//! points: the per-checkout semantic view is registered, loaded and filled
//! by the root's worker, and `aft_search` scores it.
//!
//! The embedding backend is a local HTTP mock that counts every text it is
//! asked to embed, so the tests measure real model traffic: identical content
//! in two worktrees of one repository is embedded once, a later session on
//! the same worktrees embeds nothing, and views-on answers rank exactly like
//! views-off answers on the same content.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::{AppContext, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

const DEADLINE: Duration = Duration::from_secs(90);
const FINGERPRINT_PROBE: &str = "semantic index fingerprint probe";

/// Deterministic embeddings over HTTP, counting texts other than the
/// fingerprint probe the model sends once at startup.
struct MockEmbedder {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    texts: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbedder {
    fn start() -> Self {
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
                thread::spawn(move || {
                    let _ = serve(&mut stream, &texts);
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

fn serve(stream: &mut TcpStream, texts: &AtomicUsize) -> std::io::Result<()> {
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
            .filter(|input| *input != FINGERPRINT_PROBE)
            .count(),
        Ordering::SeqCst,
    );
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
        &request(json!({
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
                    "timeout_ms": 5_000,
                    "max_batch_size": 64,
                    "max_files": 2_000
                }
            }))
        })),
        ctx,
    );
    assert!(configured.success, "configure failed: {configured:?}");
}

fn drain(ctx: &AppContext) {
    aft::runtime_drain::drain_watcher_events(ctx);
    aft::runtime_drain::drain_search_index_events(ctx);
    aft::runtime_drain::drain_semantic_index_events(ctx);
    aft::runtime_drain::drain_semantic_refresh_events(ctx);
}

/// Waits until the views-on lane is loaded and every semantic file of the
/// checkout has current vectors, and the lexical lane answers from the
/// checkout's lexical view.
fn wait_views_filled(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let complete = ctx.checkout_semantic_runtime().is_some_and(|runtime| {
            runtime
                .search(&vector("probe"), 1, &|_| true)
                .is_ok_and(|answer| answer.complete())
        });
        if complete && lexical_view_serves(ctx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "views-on semantic view never filled: status={:?}",
            ctx.semantic_index_status().read().unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// With views on, the legacy search index is never loaded: the lexical lane
/// is the checkout's lexical view, bound on demand by each query. A grep that
/// succeeds and counts as a lexical-view route shows that lane is ready.
fn lexical_view_serves(ctx: &AppContext) -> bool {
    let before = ctx.lexical_view_route_counts()[0];
    let answer = aft::commands::grep::handle_grep(
        &request(json!({"id": "lexical-ready", "command": "grep", "pattern": "fn"})),
        ctx,
    );
    answer.success && ctx.lexical_view_route_counts()[0] > before
}

fn wait_legacy_ready(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let semantic = matches!(
            &*ctx.semantic_index_status().read().unwrap(),
            SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
        );
        let lexical = ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready);
        if semantic && lexical {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "legacy indexes never became ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
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

#[test]
fn prose_watchdog_query_reports_views_coverage_and_keeps_symbol_previews() {
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
    let ctx = configure(&repo.main, storage.path(), &server, true);
    wait_views_filled(&ctx);
    let runtime = ctx.checkout_semantic_runtime().unwrap();
    let query = "how does the bash watchdog kill a background task that exceeded its timeout";
    let query_vector = vector(&format!("views-semantic-mock\u{0}{query}"));
    let vectors = runtime.index().unwrap().len();
    let raw = runtime.search(&query_vector, 100, &|_| true).unwrap();
    let answer = search(&ctx, query);
    let audit = &answer["recall_audit"]["semantic_preview"];
    let mut diagnostics = format!(
        "repository root={:?}, configured root={:?}, cache root={:?}\n",
        repo.main,
        ctx.config().project_root,
        ctx.canonical_cache_root_opt()
    );
    for section in ["metadata", "symbol_metadata", "ranked_page"] {
        diagnostics.push_str(&format!("semantic preview {section}:\n"));
        if let Some(rows) = audit[section].as_array() {
            for row in rows {
                diagnostics.push_str(&format!(
                    "  path={} details={}\n",
                    row["path_debug"].as_str().unwrap_or("<missing Debug path>"),
                    row
                ));
            }
        } else {
            diagnostics.push_str("  <missing audit section>\n");
        }
    }
    diagnostics.push_str(&format!("raw semantic hits: {:#?}\n", raw.results));
    eprintln!("watchdog reproduction: vectors={vectors}, raw_semantic_hits={}, pending={}, failed={}, complete={}\n{}", raw.results.len(), raw.pending.len(), raw.failed.len(), raw.complete(), answer["text"]);
    assert!(
        audit["metadata"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "missing semantic metadata diagnostics\n{diagnostics}\n{answer:#}"
    );
    assert_eq!(
        audit["ranked_page"].as_array().map(Vec::len),
        answer["results"].as_array().map(Vec::len),
        "missing ranked-path diagnostics\n{diagnostics}\n{answer:#}"
    );
    assert!(vectors > 0 && !raw.results.is_empty(), "{diagnostics}");
    assert_eq!(answer["complete"], true, "{answer:#}");
    let text = answer["text"].as_str().unwrap();
    assert!(
        text.contains("kill_expired_task [function] lines 3-5"),
        "{text}\n{diagnostics}"
    );
    assert!(text.contains("terminate_child();"), "{text}\n{diagnostics}");
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

#[test]
fn views_on_worktrees_and_sessions_embed_each_chunk_once() {
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let chunks = unique_chunks(&repo.main);
    assert!(chunks > 0);

    // Session 1: both worktrees, sharing one storage root.
    {
        let main = configure(&repo.main, storage.path(), &server, true);
        wait_views_filled(&main);
        let linked = configure(&repo.linked, storage.path(), &server, true);
        wait_views_filled(&linked);
        // `wait_views_filled` scores with a local vector, so the mock has
        // counted fill texts only.
        assert_eq!(
            server.texts(),
            chunks,
            "two worktrees with identical content must embed each chunk once"
        );
        assert!(
            main.semantic_index().read().unwrap().is_none(),
            "a views-on root must not build the legacy semantic index"
        );
    }

    // Session 2 on the same worktrees, after the first one ended.
    let embedded = server.texts();
    let main = configure(&repo.main, storage.path(), &server, true);
    wait_views_filled(&main);
    let linked = configure(&repo.linked, storage.path(), &server, true);
    wait_views_filled(&linked);
    assert_eq!(
        server.texts(),
        embedded,
        "a later session re-embedded content its family already holds"
    );
}

/// The model fingerprint of the root's served semantic view, if any.
fn served_model(ctx: &AppContext) -> Option<String> {
    ctx.checkout_semantic_runtime().map(|runtime| {
        runtime
            .plane()
            .semantic_producer()
            .model_fingerprint
            .clone()
    })
}

/// Waits until exactly one lane worker is left for the root and no other
/// starts within a short settle time.
fn wait_single_worker(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    while ctx.checkout_semantic().workers() != 1 {
        assert!(
            Instant::now() < deadline,
            "lane workers never settled to one: {}",
            ctx.checkout_semantic().workers()
        );
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(ctx.checkout_semantic().workers(), 1);
}

/// Reconfiguring a views-on root with another embedding model restarts its
/// semantic lane with the new producer: every chunk is embedded again under
/// that model, one worker is left, and the answers rank exactly like a
/// views-off root on the new model (the mock's vectors depend on the model,
/// so any vector of the old model would change them).
#[test]
fn views_on_model_change_restarts_the_lane_with_the_new_producer() {
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let chunks = unique_chunks(&repo.main);
    let ctx = configure(&repo.main, storage.path(), &server, true);
    wait_views_filled(&ctx);
    let old_model = served_model(&ctx).unwrap();
    assert_eq!(server.texts(), chunks);

    send_configure(
        &ctx,
        &repo.main,
        storage.path(),
        &server,
        true,
        "views-semantic-mock-2",
    );
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let started = Instant::now();
    let deadline = started + DEADLINE;
    while served_model(&ctx).is_none_or(|model| model == old_model) {
        drain(&ctx);
        assert!(
            Instant::now() < deadline,
            "the lane never restarted after {:?}: lane={} status={:?}",
            started.elapsed(),
            ctx.checkout_semantic().describe(),
            ctx.semantic_index_status().read().unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    }
    wait_views_filled(&ctx);
    wait_single_worker(&ctx);
    assert_eq!(
        server.texts(),
        2 * chunks,
        "the new model must embed every chunk once, reusing nothing of the old one"
    );

    let legacy_storage = tempfile::tempdir().unwrap();
    let legacy = Arc::new(
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        )
        .with_test_checkpoint_store(),
    );
    send_configure(
        &legacy,
        &repo.main,
        legacy_storage.path(),
        &server,
        false,
        "views-semantic-mock-2",
    );
    aft::runtime_drain::drain_deferred_configure_maintenance(&legacy);
    wait_legacy_ready(&legacy);
    for query in [
        "evict the oldest cache entry",
        "exponential backoff between retries",
    ] {
        let expected = ranked(&repo.main, &search(&legacy, query));
        let actual = search(&ctx, query);
        assert_eq!(actual["complete"], true, "{actual:#}");
        assert_eq!(
            ranked(&repo.main, &actual),
            expected,
            "views-on after a model change ranks differently for {query:?}"
        );
    }
}

/// Two configures in quick succession leave one lane worker, serving the
/// second configure's model; the superseded lane embeds nothing.
#[test]
fn views_on_back_to_back_configures_leave_one_worker() {
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let chunks = unique_chunks(&repo.main);
    let ctx = Arc::new(
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        )
        .with_test_checkpoint_store(),
    );
    send_configure(
        &ctx,
        &repo.main,
        storage.path(),
        &server,
        true,
        "first-model",
    );
    send_configure(
        &ctx,
        &repo.main,
        storage.path(),
        &server,
        true,
        "second-model",
    );
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    wait_views_filled(&ctx);
    wait_single_worker(&ctx);
    assert!(
        served_model(&ctx).is_some_and(|model| model.contains("second-model")),
        "the lane serves {:?}",
        served_model(&ctx)
    );
    assert_eq!(
        server.texts(),
        chunks,
        "the superseded lane embedded content"
    );
}

#[test]
fn views_on_search_ranks_like_views_off_on_the_same_content() {
    let server = MockEmbedder::start();
    let repo = repository();
    let legacy_storage = tempfile::tempdir().unwrap();
    let views_storage = tempfile::tempdir().unwrap();

    let legacy = configure(&repo.main, legacy_storage.path(), &server, false);
    wait_legacy_ready(&legacy);
    let views = configure(&repo.main, views_storage.path(), &server, true);
    wait_views_filled(&views);

    for query in [
        "evict the oldest cache entry",
        "read a configuration file from disk",
        "exponential backoff between retries",
    ] {
        let expected = search(&legacy, query);
        let actual = search(&views, query);
        assert_eq!(actual["complete"], true, "views-on incomplete: {actual:#}");
        let expected_rows = ranked(&repo.main, &expected);
        assert!(
            !expected_rows.is_empty(),
            "views-off found nothing for {query}"
        );
        assert_eq!(
            ranked(&repo.main, &actual),
            expected_rows,
            "views-on ranking differs for {query:?}"
        );
    }
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

/// Waits until the root's views-on semantic view scores a chunk named
/// `name` with nothing missing, and no longer scores `gone`. The mock's
/// vectors carry no meaning, so this asks for every chunk rather than for a
/// rank. It scores with a local query vector, so the mock's text count covers
/// fills only.
fn wait_for_row(ctx: &AppContext, name: &str, gone: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let answer = ctx
            .checkout_semantic_runtime()
            .expect("views-on semantic view")
            .search(&vector("any query"), 1_000, &|_| true)
            .unwrap();
        let names = answer
            .results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>();
        if answer.complete() && names.contains(&name) && !names.contains(&gone) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never replaced {gone}: {answer:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn views_on_aft_and_outside_edits_reach_the_fill() {
    let _serial = crate::helpers::watcher_serial_lock();
    let _quiet = EnvGuard(
        "AFT_SEMANTIC_QUIET_WINDOW_MS",
        std::env::var_os("AFT_SEMANTIC_QUIET_WINDOW_MS"),
    );
    // SAFETY: changed only while `watcher_serial_lock` is held; restored by
    // the guard above.
    unsafe { std::env::set_var("AFT_SEMANTIC_QUIET_WINDOW_MS", "50") };
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let ctx = configure(&repo.main, storage.path(), &server, true);
    wait_views_filled(&ctx);

    // An edit through AFT's own write command.
    let before = server.texts();
    let written = aft::commands::write::handle_write(
        &request(json!({
            "id": "views-semantic-write",
            "command": "write",
            "file": repo.main.join("src/retry.rs"),
            "content": "pub fn jittered_sleep_between_attempts(attempt: u32) -> u64 {\n    u64::from(attempt) * 10\n}\n"
        })),
        &ctx,
    );
    assert!(written.success, "write failed: {written:?}");
    wait_for_row(
        &ctx,
        "jittered_sleep_between_attempts",
        "retry_with_backoff",
    );
    assert!(server.texts() > before, "the AFT write was never embedded");

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
    wait_for_row(&ctx, "tokenize_shell_arguments", "parse_config_file");
    assert!(
        server.texts() > before,
        "the outside edit was never embedded"
    );
}

/// The fields of a search answered while the semantic lane is reloading after
/// an idle eviction: building, incomplete, lexical results with the lane's
/// disclosure, and never a ready lane.
fn assert_reloading_after_eviction(answer: &Value) {
    assert_eq!(answer["semantic_status"], "building", "{answer:#}");
    assert_eq!(answer["complete"], false, "{answer:#}");
    assert_eq!(answer["lexical_only_fallback"], true, "{answer:#}");
    assert!(
        answer["note"].as_str().is_some_and(|note| !note.is_empty()),
        "the reply must disclose the lexical fallback: {answer:#}"
    );
    let text = answer["text"].as_str().unwrap_or_default();
    assert!(
        !text.lines().any(|line| line == "semantic: ready"),
        "the reply must not claim a ready semantic lane: {text}"
    );
}

/// Evicts the root's artifacts as the idle reaper does until its views-on
/// semantic lane is released. Eviction does not wait for a lane whose locks
/// are held (its worker can still be publishing the fill the test waited
/// for): it leaves that lane loaded, and the reaper releases it on a later
/// pass. This repeats the eviction the same way.
fn evict_until_lane_released(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(ctx.evict_idle_artifacts(), "the root must be idle");
        if !ctx.checkout_semantic().active() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "idle eviction never released the lane: {}",
            ctx.checkout_semantic().describe()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
/// Evicts the root's artifacts once it is idle. Readiness can be reported
/// while background work that settled it is still finishing, and eviction
/// refuses a root that is not idle yet, so retry until it accepts.
fn evict_once_idle(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    while !ctx.evict_idle_artifacts() {
        assert!(Instant::now() < deadline, "the root never became idle");
        thread::sleep(Duration::from_millis(20));
    }
}
/// Idle eviction drops the lexical lane too, and both lanes reload by design.
/// Reloads the lexical lane through a grep, which reloads only that lane, and
/// waits until grep answers from a ready index, so the next search has a
/// ready lexical lane however long the reload takes on this machine while the
/// semantic lane is still released. With views on, that lane is the
/// checkout's v3 lexical view, which the grep binds again; with views off it
/// is the legacy trigram index.
fn reload_trigram_after_eviction(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let grep = aft::commands::grep::handle_grep(
            &request(json!({
                "id": "views-semantic-grep",
                "command": "grep",
                "pattern": "parse_config_file",
                "max_results": 1
            })),
            ctx,
        );
        assert!(grep.success, "grep failed: {grep:?}");
        aft::runtime_drain::drain_search_index_events(ctx);
        let ready = if ctx.config().views.enabled {
            grep.data["index_status"] == "Ready"
        } else {
            ctx.search_index()
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|index| index.ready)
        };
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the lexical lane never reloaded after eviction: {:?}",
            grep.data
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// After an idle eviction releases a views-on root's semantic lane, the next
/// search restarts the lane (the reload a views-off root's evicted index gets
/// too) and that same search reports the lane as building with the
/// lexical-fallback disclosure, not as ready. Once the view is back, searches
/// are ready again.
#[test]
fn views_on_search_after_idle_eviction_reports_the_lane_reloading() {
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let ctx = configure(&repo.main, storage.path(), &server, true);
    wait_views_filled(&ctx);
    evict_until_lane_released(&ctx);
    reload_trigram_after_eviction(&ctx);
    assert!(
        !ctx.checkout_semantic().active(),
        "reloading the trigram index must not restart the semantic lane: {}",
        ctx.checkout_semantic().describe()
    );

    let query = "read a configuration file from disk";
    assert_reloading_after_eviction(&search(&ctx, query));
    assert!(
        ctx.checkout_semantic().active(),
        "the search must restart the lane: {}",
        ctx.checkout_semantic().describe()
    );
    wait_views_filled(&ctx);
    let deadline = Instant::now() + DEADLINE;
    while !matches!(
        &*ctx.semantic_index_status().read().unwrap(),
        SemanticIndexStatus::Ready { .. }
    ) {
        assert!(
            Instant::now() < deadline,
            "the reloaded lane never became ready"
        );
        drain(&ctx);
        thread::sleep(Duration::from_millis(20));
    }
    let answer = search(&ctx, query);
    assert_eq!(answer["semantic_status"], "ready", "{answer:#}");
    assert_eq!(answer["complete"], true, "{answer:#}");
}

/// A views-off root reloads its evicted semantic index on the next search,
/// and that same search reports the lane as building with the lexical-fallback
/// disclosure, not as the ready its evicted index left behind.
#[test]
fn views_off_search_after_idle_eviction_reports_the_index_reloading() {
    let server = MockEmbedder::start();
    let repo = repository();
    let storage = tempfile::tempdir().unwrap();
    let ctx = configure(&repo.main, storage.path(), &server, false);
    wait_legacy_ready(&ctx);
    evict_once_idle(&ctx);
    // As in the views-on test: a search would refuse while the text index is
    // still reloading too, so reload it first and keep the check on semantic.
    reload_trigram_after_eviction(&ctx);
    assert_reloading_after_eviction(&search(&ctx, "read a configuration file from disk"));
}
