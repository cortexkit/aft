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
    git(
        &main,
        &["worktree", "add", "-q", linked.to_str().unwrap(), "HEAD"],
    );
    Repository {
        _temp: temp,
        main,
        linked,
    }
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).unwrap()
}

fn configure(root: &Path, storage: &Path, server: &MockEmbedder, views: bool) -> Arc<AppContext> {
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    let configured = aft::commands::configure::handle_configure(
        &request(json!({
            "id": "configure-views-semantic",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(json!({
                "search_index": true,
                "semantic_search": true,
                "callgraph_store": false,
                "views": { "enabled": views },
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "views-semantic-mock",
                    "base_url": server.base_url,
                    "timeout_ms": 5_000,
                    "max_batch_size": 64,
                    "max_files": 2_000
                }
            }))
        })),
        &ctx,
    );
    assert!(configured.success, "configure failed: {configured:?}");
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    ctx
}

fn drain(ctx: &AppContext) {
    aft::runtime_drain::drain_watcher_events(ctx);
    aft::runtime_drain::drain_search_index_events(ctx);
    aft::runtime_drain::drain_semantic_index_events(ctx);
    aft::runtime_drain::drain_semantic_refresh_events(ctx);
}

/// Waits until the views-on lane is loaded and every semantic file of the
/// checkout has current vectors.
fn wait_views_filled(ctx: &AppContext) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let complete = ctx.checkout_semantic_runtime().is_some_and(|runtime| {
            runtime
                .search(&vector("probe"), 1, &|_| true)
                .is_ok_and(|answer| answer.complete())
        });
        let lexical = ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready);
        if complete && lexical {
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
    serde_json::to_value(aft::commands::semantic_search::handle_semantic_search(
        &request(json!({ "id": "views-semantic-search", "command": "search", "query": query })),
        ctx,
    ))
    .unwrap()
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
