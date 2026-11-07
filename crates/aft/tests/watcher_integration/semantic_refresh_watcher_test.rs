use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
#[cfg(feature = "test-timing-hooks")]
use std::path::PathBuf;
#[cfg(feature = "test-timing-hooks")]
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::Duration;

use aft::config::SemanticBackendConfig;
use aft::context::SemanticIndexStatus;
use aft::search_index::SearchIndex;
use aft::semantic_index::{EmbeddingModel, QueryBudget, SemanticIndex};
use serde_json::{json, Value};

use crate::helpers::{user_config, AftProcess};

#[derive(Default)]
struct RefreshGate {
    // (request entered, response released)
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl RefreshGate {
    fn hold(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn wait_until_held(&self) {
        let (state, _) = self
            .changed
            .wait_timeout_while(
                self.state.lock().unwrap(),
                Duration::from_secs(30),
                |state| !state.0,
            )
            .unwrap();
        let (entered, released) = *state;
        drop(state);
        assert!(entered, "refresh embedding request never reached the mock");
        assert!(
            !released,
            "refresh must remain held until explicitly released"
        );
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

struct MockEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    // Keep the refresh in flight until explicit release, including during a
    // slow query. Drop releases the gate if an assertion panics.
    refresh_gate: Arc<RefreshGate>,
    // Read only by the linked-worktree quiet-window test, which needs the
    // `test-timing-hooks` feature; the server records into it regardless.
    #[cfg_attr(not(feature = "test-timing-hooks"), allow(dead_code))]
    requests: Arc<Mutex<Vec<Vec<String>>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let refresh_gate = Arc::new(RefreshGate::default());
        let release_for_thread = Arc::clone(&refresh_gate);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Serve each connection on its own thread. The refresh
                        // request is deliberately held open while the test
                        // asserts mid-refresh queryability, and a serial accept
                        // loop would starve the concurrent query embed behind
                        // it (real backends serve requests concurrently).
                        let release = Arc::clone(&release_for_thread);
                        let requests = Arc::clone(&requests_for_thread);
                        thread::spawn(move || {
                            let _ = handle_embedding_request(&mut stream, &release, &requests);
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            refresh_gate,
            requests,
            handle: Some(handle),
        }
    }

    /// Release the held post-edit refresh embedding request. Call this once the
    /// test has observed `refreshing_count == 1` so the refresh can complete.
    fn release_refresh(&self) {
        self.refresh_gate.release();
    }

    #[cfg(feature = "test-timing-hooks")]
    fn reset_requests(&self) {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    #[cfg(feature = "test-timing-hooks")]
    fn non_probe_requests(&self) -> Vec<Vec<String>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|inputs| {
                inputs
                    .iter()
                    .any(|input| input != "semantic index fingerprint probe")
            })
            .cloned()
            .collect()
    }
}

impl Drop for MockEmbeddingServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.release_refresh();
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("embedding server thread");
        }
    }
}

fn handle_embedding_request(
    stream: &mut TcpStream,
    refresh_gate: &RefreshGate,
    requests: &Arc<Mutex<Vec<Vec<String>>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;

    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = Some(pos + 4);
                for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                    let Some((name, value)) = line.split_once(':') else {
                        continue;
                    };
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                }
            }
        }
        if let Some(end) = header_end {
            if buf.len() >= end + content_length {
                break;
            }
        }
    }

    let body = header_end
        .and_then(|end| buf.get(end..end + content_length))
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .unwrap_or_else(|| json!({ "input": [] }));
    let inputs = match &body["input"] {
        Value::Array(values) => values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        Value::String(value) => vec![value.clone()],
        _ => Vec::new(),
    };
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(inputs.clone());

    if inputs
        .iter()
        .any(|input| input.to_ascii_lowercase().contains("after edit refreshed"))
    {
        refresh_gate.hold();
    }

    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| json!({ "embedding": embedding_for(input), "index": index }))
        .collect::<Vec<_>>();
    let body = json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())
}

fn embedding_for(text: &str) -> Vec<f32> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("alpha_anchor") || lower.contains("alpha anchor") {
        vec![1.0, 0.0, 0.0]
    } else if lower.contains("edited_refresh_marker") || lower.contains("edited refresh marker") {
        vec![0.0, 1.0, 0.0]
    } else {
        vec![0.0, 0.0, 1.0]
    }
}

#[cfg(feature = "test-timing-hooks")]
fn semantic_inputs_for_file(root: &Path, source: &Path) -> Vec<String> {
    let canonical_root = fs::canonicalize(root).expect("canonical semantic root");
    let canonical_source = fs::canonicalize(source).expect("canonical semantic source");
    let mut inputs = Vec::new();
    let mut embed = |texts: Vec<String>| {
        inputs.extend(texts.iter().cloned());
        Ok::<_, String>(
            texts
                .iter()
                .map(|text| embedding_for(text))
                .collect::<Vec<_>>(),
        )
    };
    SemanticIndex::build(&canonical_root, &[canonical_source], &mut embed, 64)
        .expect("collect expected semantic inputs");
    inputs
}

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create project dir");
    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, content).expect("write fixture");
    }
    temp_dir
}

#[cfg(feature = "test-timing-hooks")]
fn run_git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.arg("-C").arg(root));
    let status = command.args(args).status().expect("run git command");
    assert!(
        status.success(),
        "git {args:?} failed in {}",
        root.display()
    );
}

#[cfg(feature = "test-timing-hooks")]
fn setup_linked_worktree(source: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let fixture = tempfile::tempdir().expect("create linked-worktree fixture");
    let main = fixture.path().join("main");
    let worktree = fixture.path().join("worktree");
    fs::create_dir_all(main.join("src")).expect("create main source directory");
    fs::write(main.join("src/lib.rs"), source).expect("write tracked source");
    run_git(&main, &["init", "--quiet"]);
    run_git(&main, &["add", "."]);

    let mut commit = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(commit.arg("-C").arg(&main));
    let status = commit
        .args([
            "-c",
            "user.name=AFT Test",
            "-c",
            "user.email=aft@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ])
        .status()
        .expect("commit linked-worktree fixture");
    assert!(status.success(), "fixture commit failed");

    let worktree_text = worktree.to_string_lossy().into_owned();
    run_git(
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &worktree_text,
            "HEAD",
        ],
    );
    (fixture, main, worktree)
}

#[cfg(unix)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn configure_semantic_openai(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    base_url: &str,
) -> Value {
    configure_semantic_openai_with_overlay(aft, root, storage_dir, base_url, false)
}

fn configure_semantic_openai_with_overlay(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    base_url: &str,
    ram_overlay: bool,
) -> Value {
    send(
        aft,
        json!({
            "id": "cfg-semantic-refresh",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "storage_dir": storage_dir.display().to_string(),
            "config": user_config(serde_json::json!({
                "indexes": { "trigram": false, "semantic": true },
                "worktree": {
                    "ram_overlay": ram_overlay
                },
                "semantic": semantic_backend_config(base_url)
            })),
        }),
    )
}

fn semantic_backend_config(base_url: &str) -> Value {
    json!({
        "backend": "openai_compatible",
        "model": "test-embedding",
        "base_url": base_url,
        "timeout_ms": 30_000,
        // Query embeddings have a separate 3s production deadline. This mock
        // tests refresh queryability, not runner scheduling latency; use the
        // maximum query budget as a hang guard, just like the build timeout.
        "query_timeout_ms": 30_000,
        "max_batch_size": 64,
        "max_files": 20_000
    })
}

#[test]
fn semantic_refresh_fixture_uses_query_hang_guard() {
    let config: SemanticBackendConfig =
        serde_json::from_value(semantic_backend_config("http://127.0.0.1:9")).unwrap();
    assert_eq!(config.timeout_ms, 30_000);
    assert_eq!(
        config.query_timeout_ms, 30_000,
        "the watcher scenario tests queryability, not the interactive latency budget"
    );
}

#[test]
fn semantic_refresh_fixture_embeds_query_while_refresh_is_held() {
    let server = MockEmbeddingServer::start();
    let config: SemanticBackendConfig =
        serde_json::from_value(semantic_backend_config(&server.base_url)).unwrap();
    let mut refresh_model = EmbeddingModel::from_config(&config).unwrap();
    let refresh = thread::spawn(move || {
        refresh_model.embed(vec!["after edit refreshed content".to_string()])
    });
    server.refresh_gate.wait_until_held();

    let mut query_model = EmbeddingModel::from_config_for_query(&config).unwrap();
    let query = query_model
        .embed_query_cached("alpha anchor", QueryBudget::from_config(&config))
        .expect("query must embed independently of the held refresh");
    assert_eq!(query, embedding_for("alpha anchor"));
    server.refresh_gate.wait_until_held();
    assert!(!refresh.is_finished(), "refresh completed before release");

    server.release_refresh();
    refresh.join().unwrap().expect("released refresh embeds");
}

fn status(aft: &mut AftProcess) -> Value {
    send(
        aft,
        json!({
            "id": "status",
            "command": "status",
        }),
    )
}

/// A mock embedding endpoint that starts "down" (returns HTTP 503, a transient
/// status) and flips to serving real embeddings once `bring_up()` is called.
/// Used to prove the build-level retry rides out a transient backend outage and
/// self-heals — instead of parking the index in `Failed` forever.
struct FlakyEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    up: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FlakyEmbeddingServer {
    fn start_down() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind flaky embedding server");
        let addr = listener.local_addr().expect("flaky embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let up = Arc::new(AtomicBool::new(false));
        let running_for_thread = Arc::clone(&running);
        let up_for_thread = Arc::clone(&up);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let serve = up_for_thread.load(Ordering::SeqCst);
                        let _ = handle_flaky_request(&mut stream, serve);
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            up,
            handle: Some(handle),
        }
    }

    fn bring_up(&self) {
        self.up.store(true, Ordering::SeqCst);
    }
}

impl Drop for FlakyEmbeddingServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("flaky embedding server thread");
        }
    }
}

fn handle_flaky_request(stream: &mut TcpStream, serve: bool) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos + 4);
                for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
            }
        }
        if let Some(end) = header_end {
            if buf.len() >= end + content_length {
                break;
            }
        }
    }

    if !serve {
        // 503 is a server error -> classified transient -> the build keeps
        // retrying with backoff instead of failing.
        let response =
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        return stream.write_all(response.as_bytes());
    }

    let body = header_end
        .and_then(|end| buf.get(end..end + content_length))
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .unwrap_or_else(|| json!({ "input": [] }));
    let inputs = match &body["input"] {
        Value::Array(values) => values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        Value::String(value) => vec![value.clone()],
        _ => Vec::new(),
    };
    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| json!({ "embedding": embedding_for(input), "index": index }))
        .collect::<Vec<_>>();
    let body = json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())
}

fn wait_for_semantic_status<F>(aft: &mut AftProcess, label: &str, predicate: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    // Generous budget: this e2e test waits on an OS FSEvents delivery (a watched
    // write reaching the spawned aft process) then a refresh worker reacting.
    // The loop returns immediately on match (~2s happy path), so a high cap
    // costs nothing when passing and only buys headroom. The real defense
    // against gate flakiness is `watcher_serial_lock` (these fseventsd-delivery
    // tests run one at a time so the daemon isn't swamped); this budget only
    // absorbs ordinary scheduling latency on top of that.
    let mut last_response = None;
    for _ in 0..400 {
        let response = status(aft);
        assert_eq!(
            response["success"], true,
            "status should succeed while waiting for {label}: {response:?}"
        );
        if predicate(&response) {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!(
        "semantic status did not become {label} in time; last response: {:?}",
        last_response
    );
}

/// Like `wait_for_semantic_status`, but re-writes `contents` to `file` on every
/// poll until `predicate` holds. This defeats FSEvents watcher attach latency:
/// the recursive watcher comes up asynchronously after configure, so a single
/// pre-attach write can be missed entirely. Re-emitting the modify event each
/// iteration guarantees the watcher eventually observes a change once it is
/// live, without depending on one perfectly-timed write.
fn wait_for_semantic_status_with_retouch<F>(
    aft: &mut AftProcess,
    label: &str,
    file: &Path,
    contents: &str,
    predicate: F,
) -> Value
where
    F: Fn(&Value) -> bool,
{
    let mut last_response = None;
    for i in 0..400 {
        let response = status(aft);
        assert_eq!(
            response["success"], true,
            "status should succeed while waiting for {label}: {response:?}"
        );
        if predicate(&response) {
            return response;
        }
        // Re-touch periodically (not every poll) so we keep emitting modify
        // events until the watcher attaches, without hammering the filesystem.
        // Each re-touch writes UNIQUE content (a varying trailing comment) so
        // the refresh's content-hash freshness check never dedups it away. With
        // identical bytes, if the watcher dropped/coalesced the first event under
        // parallel-suite load, every later re-touch hashed equal to the already-
        // seen content and the refresh could never fire — the root cause of this
        // test's flakiness. The asserted marker lives in the symbol body, which
        // `contents` preserves; the trailing comment only perturbs the hash.
        if i % 3 == 0 {
            let _ = fs::write(file, format!("{contents}// retouch {i}\n"));
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!(
        "semantic status did not become {label} in time (with retouch); last response: {:?}",
        last_response
    );
}

#[test]
fn refreshing_status_keeps_repeated_same_file_invalidations_until_last_completion() {
    let file = Path::new("src/repeated.rs").to_path_buf();
    let mut status = SemanticIndexStatus::ready();

    status.add_refreshing_file(file.clone());
    status.start_refreshing_file(file.clone());
    status.add_refreshing_file(file.clone());

    assert_eq!(status.refreshing_count(), 1);
    let SemanticIndexStatus::Ready { refreshing, .. } = &status else {
        panic!("semantic status should stay ready");
    };
    assert_eq!(refreshing.as_slice(), std::slice::from_ref(&file));

    status.complete_refreshing_file(&file);

    assert_eq!(
        status.refreshing_count(),
        1,
        "first refresh completion must not clear a queued refresh for the same file"
    );
    let SemanticIndexStatus::Ready { refreshing, .. } = &status else {
        panic!("semantic status should stay ready");
    };
    assert_eq!(refreshing.as_slice(), std::slice::from_ref(&file));

    status.start_refreshing_file(file.clone());
    status.complete_refreshing_file(&file);

    assert_eq!(status.refreshing_count(), 0);
    let SemanticIndexStatus::Ready { refreshing, .. } = &status else {
        panic!("semantic status should stay ready");
    };
    assert!(refreshing.is_empty());
}

#[cfg(feature = "test-timing-hooks")]
#[test]
fn linked_worktree_semantic_embedding_respects_ram_overlay_and_quiet_window() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let original = "pub fn shared_owner_baseline() -> bool { true }\n";
    let (_fixture, main, worktree) = setup_linked_worktree(original);
    let storage = tempfile::tempdir().expect("create storage dir");
    let server = MockEmbeddingServer::start();

    let mut owner = AftProcess::spawn();
    let configured = configure_semantic_openai(&mut owner, &main, storage.path(), &server.base_url);
    assert_eq!(
        configured["success"], true,
        "owner configure: {configured:?}"
    );
    wait_for_semantic_status(&mut owner, "owner semantic index", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["entries"].as_u64().unwrap_or(0) > 0
    });
    assert!(owner.shutdown().success());
    server.reset_requests();

    let quiet_ms = std::ffi::OsStr::new("2000");
    let mut borrower =
        AftProcess::spawn_with_real_watcher_env(&[("AFT_SEMANTIC_QUIET_WINDOW_MS", quiet_ms)]);
    let configured = configure_semantic_openai_with_overlay(
        &mut borrower,
        &worktree,
        storage.path(),
        &server.base_url,
        false,
    );
    assert_eq!(
        configured["success"], true,
        "borrow configure: {configured:?}"
    );
    wait_for_semantic_status(&mut borrower, "borrowed semantic index", |response| {
        response["cache_role"] == "worktree" && response["semantic_index"]["status"] == "ready"
    });
    assert!(
        server.non_probe_requests().is_empty(),
        "borrow-only bind must not call the embedder"
    );

    let source = worktree.join("src/lib.rs");
    fs::write(
        &source,
        "pub fn borrow_only_edit_is_masked() -> bool { false }\n",
    )
    .expect("edit borrow-only source");
    wait_for_semantic_status(&mut borrower, "borrow-only semantic mask", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["entries"] == 0
    });

    let storm = worktree.join("storm");
    fs::create_dir_all(&storm).expect("create borrow-only storm");
    for index in 0..1_470 {
        fs::write(
            storm.join(format!("appeared_{index}.rs")),
            format!("pub fn borrow_storm_{index}() {{}}\n"),
        )
        .expect("write borrow-only storm file");
    }
    fs::remove_dir_all(&storm).expect("remove borrow-only storm");
    assert!(borrower.shutdown().success());
    assert!(
        server.non_probe_requests().is_empty(),
        "borrow-only edit and appear/vanish storm must not call the embedder"
    );

    fs::write(&source, original).expect("restore shared baseline before overlay bind");
    server.reset_requests();
    let gate = storage.path().join("semantic-quiet-gate");
    let mut overlay = AftProcess::spawn_with_real_watcher_env(&[
        ("AFT_SEMANTIC_QUIET_WINDOW_MS", quiet_ms),
        ("AFT_TEST_SEMANTIC_QUIET_GATE", gate.as_os_str()),
    ]);
    let configured = configure_semantic_openai_with_overlay(
        &mut overlay,
        &worktree,
        storage.path(),
        &server.base_url,
        true,
    );
    assert_eq!(
        configured["success"], true,
        "overlay configure: {configured:?}"
    );
    wait_for_semantic_status(&mut overlay, "overlay semantic index", |response| {
        response["cache_role"] == "worktree" && response["semantic_index"]["status"] == "ready"
    });
    assert!(
        server.non_probe_requests().is_empty(),
        "RAM-overlay bind must not run a corpus catch-up"
    );

    let overlay_contents =
        "pub fn overlay_changed_file_only() -> &'static str { \"overlay delta\" }\n";
    fs::write(&source, overlay_contents).expect("edit overlay source");
    let expected_inputs = semantic_inputs_for_file(&worktree, &source);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while server.non_probe_requests().is_empty() && std::time::Instant::now() < deadline {
        let _ = status(&mut overlay);
        thread::sleep(Duration::from_millis(100));
    }
    wait_for_semantic_status(&mut overlay, "overlay semantic refresh", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["refreshing_count"] == 0
    });
    let actual_inputs = server
        .non_probe_requests()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        actual_inputs, expected_inputs,
        "overlay refresh must embed exactly the changed file's chunks"
    );

    server.reset_requests();
    fs::write(&gate, b"hold").expect("hold quiet window");
    let release_gate = crate::helpers::ReleaseOnDrop::new(gate.with_extension("release"));
    fs::create_dir_all(&storm).expect("create overlay storm");
    for index in 0..1_470 {
        fs::write(
            storm.join(format!("appeared_{index}.rs")),
            format!("pub fn overlay_storm_{index}() {{}}\n"),
        )
        .expect("write overlay storm file");
        if index == 0 {
            let cap = std::time::Instant::now() + Duration::from_secs(60);
            while !gate.with_extension("waiting").exists() {
                assert!(
                    std::time::Instant::now() < cap,
                    "refresh never observed quiet gate"
                );
                let _ = status(&mut overlay);
                thread::sleep(Duration::from_millis(10));
            }
            // Deliberately exceed the real quiet interval while the first storm
            // file remains alive. The gate, not writer speed, prevents embedding.
            thread::sleep(Duration::from_millis(2_500));
        }
    }
    fs::remove_dir_all(&storm).expect("remove overlay storm");
    fs::remove_file(&gate).expect("release quiet window");
    drop(release_gate);
    let cap = std::time::Instant::now() + Duration::from_secs(60);
    while !gate.with_extension("settled").exists() {
        assert!(
            std::time::Instant::now() < cap,
            "held refresh never completed"
        );
        let _ = status(&mut overlay);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        server.non_probe_requests().is_empty(),
        "overlay storm files gone inside the quiet window must not be embedded"
    );

    assert!(overlay.shutdown().success());
}

#[test]
fn semantic_refresh_watcher_reindexes_modified_file_and_clears_refreshing() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let project = setup_project(&[
        (
            "src/a.rs",
            "pub fn alpha_anchor() -> &'static str {\n    \"alpha anchor\"\n}\n",
        ),
        (
            "src/b.rs",
            "pub fn edited_refresh_marker() -> &'static str {\n    \"before edit\"\n}\n",
        ),
        (
            "src/c.rs",
            "pub fn gamma_helper() -> &'static str {\n    \"gamma\"\n}\n",
        ),
    ]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let server = MockEmbeddingServer::start();
    // The index store embeds an edit once the checkout has gone quiet, two
    // minutes after the last change; this test shortens that to a second.
    let mut aft = AftProcess::spawn_with_real_watcher_env(&[(
        "AFT_TEST_DATA_QUIET_MS",
        std::ffi::OsStr::new("1000"),
    )]);

    let configure =
        configure_semantic_openai(&mut aft, project.path(), storage.path(), &server.base_url);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let ready = wait_for_semantic_status(&mut aft, "initial ready", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["files_embedded"]
                .as_u64()
                .unwrap_or(0)
                >= 3
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(ready["semantic_index"]["refreshing_count"], 0);

    let edited_file = project.path().join("src/b.rs");
    let edited_contents =
        "pub fn edited_refresh_marker() -> &'static str {\n    \"after edit refreshed content\"\n}\n";
    fs::write(&edited_file, edited_contents).expect("edit watched file");

    // The mock holds the refresh embedding request open, so the file stays in
    // the "refreshing" set until we release it below — making this transient
    // state deterministically observable.
    //
    // Re-touch on every poll: the recursive FSEvents watcher attaches
    // asynchronously after configure, so under full-suite parallelism the very
    // first write can land before the watch is live (or its initial event can
    // be coalesced/dropped) and the refresh would never fire. Re-writing the
    // same content each iteration keeps emitting modify events until the watcher
    // is attached and reacts; once `refreshing_count == 1` the barrier holds it
    // open so we reliably observe it. This makes the test robust to watcher
    // attach latency instead of depending on a single well-timed event.
    let refreshing = wait_for_semantic_status_with_retouch(
        &mut aft,
        "watcher refreshing",
        &edited_file,
        edited_contents,
        |response| {
            response["semantic_index"]["status"] == "ready"
                && response["semantic_index"]["refreshing_count"] == 1
        },
    );
    assert_eq!(refreshing["semantic_index"]["refreshing_count"], 1);
    server.refresh_gate.wait_until_held();

    // Mid-refresh queryability: while the edited file's embed is held open,
    // the index must stay fully queryable (Ready, not Building), serve results
    // from unchanged files, and disclose the in-flight refresh in warnings.
    // These assertions used to live in a standalone copy of this scenario in
    // the parallel `semantic_test` binary; a real-FSEvents test cannot run
    // reliably outside this serialized binary, so the coverage moved here.
    let mid_refresh = send(
        &mut aft,
        json!({
            "id": "semantic-mid-refresh-search",
            "command": "semantic_search",
            "query": "alpha anchor",
            "hint": "semantic",
            "top_k": 5,
        }),
    );
    assert_eq!(
        mid_refresh["success"], true,
        "mid-refresh semantic search should succeed: {mid_refresh:?}"
    );
    // The reply is partial: the index serves, but the refreshing file's
    // vectors are missing until the refresh finishes. The index store names
    // that file, where today's index answered "ready" with a count warning.
    assert_eq!(mid_refresh["status"], "partial", "{mid_refresh:?}");
    assert_eq!(mid_refresh["semantic_status"], "ready", "{mid_refresh:?}");
    assert_eq!(mid_refresh["interpreted_as"], "semantic");
    let mid_text = mid_refresh["text"].as_str().unwrap_or_default();
    assert!(
        mid_text.contains("semantic: 1 file(s) not yet embedded: src/b.rs"),
        "expected the refreshing file to be named, got {mid_text:?}"
    );
    assert_eq!(mid_refresh["complete"], false, "{mid_refresh:?}");
    let mid_results = mid_refresh["results"].as_array().expect("results array");
    assert!(
        mid_results.iter().any(|result| {
            result["source"] == "semantic"
                && result["file"]
                    .as_str()
                    .is_some_and(|file| file.replace('\\', "/").ends_with("src/a.rs"))
        }),
        "expected semantic result from unchanged file, got {mid_results:?}"
    );

    // Observed and queried the refreshing state; let the refresh complete.
    server.refresh_gate.wait_until_held();
    server.release_refresh();

    let refreshed = wait_for_semantic_status(&mut aft, "refresh completed", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(refreshed["semantic_index"]["refreshing_count"], 0);

    let search = send(
        &mut aft,
        json!({
            "id": "semantic-refresh-search",
            "command": "semantic_search",
            "query": "edited refresh marker",
            "hint": "semantic",
            "top_k": 5,
        }),
    );
    assert_eq!(
        search["success"], true,
        "semantic search should succeed: {search:?}"
    );
    assert_eq!(search["status"], "ready");
    let results = search["results"].as_array().expect("results array");
    let edited_result = results
        .iter()
        .find(|result| {
            result["file"]
                .as_str()
                .is_some_and(|file| file.replace('\\', "/").ends_with("src/b.rs"))
        })
        .unwrap_or_else(|| panic!("expected refreshed src/b.rs result, got {results:?}"));
    assert!(
        edited_result["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("after edit refreshed content")),
        "expected refreshed snippet, got {edited_result:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn semantic_refresh_defers_new_files_when_max_files_cap_is_reached() {
    let project = setup_project(&[
        (
            "src/a.rs",
            "pub fn alpha_anchor() -> &'static str {\n    \"alpha anchor\"\n}\n",
        ),
        (
            "src/b.rs",
            "pub fn edited_refresh_marker() -> &'static str {\n    \"edited refresh marker\"\n}\n",
        ),
    ]);
    let a = fs::canonicalize(project.path().join("src/a.rs")).expect("canonicalize a");
    let b = fs::canonicalize(project.path().join("src/b.rs")).expect("canonicalize b");
    let root = fs::canonicalize(project.path()).expect("canonicalize project");

    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|text| embedding_for(&text)).collect())
    };
    let mut index = SemanticIndex::build(&root, std::slice::from_ref(&a), &mut embed, 64)
        .expect("build initial semantic index");
    assert_eq!(index.indexed_file_count(), 1);

    let mut embed_refresh = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|text| embedding_for(&text)).collect())
    };
    let mut progress = |_done: usize, _total: usize| {};
    let update = index
        .refresh_invalidated_files(
            &root,
            std::slice::from_ref(&b),
            &mut embed_refresh,
            64,
            1,
            &mut progress,
        )
        .expect("refresh should succeed while deferring the new file");

    assert_eq!(update.summary.added, 0);
    assert_eq!(index.indexed_file_count(), 1);
    assert!(
        index
            .search(&[0.0, 1.0, 0.0], 10)
            .iter()
            .all(|result| result.file != b),
        "deferred file should not be searchable"
    );
}

#[test]
fn watcher_deleted_alias_path_invalidates_canonical_search_and_semantic_entries() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let real_root = temp_dir.path().join("real-project");
    let source_file = real_root.join("src/lib.rs");
    fs::create_dir_all(source_file.parent().expect("source parent")).expect("create src dir");
    fs::write(
        &source_file,
        "pub fn alias_delete_anchor() -> usize {
    42
}
",
    )
    .expect("write indexed source");

    let alias_root = temp_dir.path().join("alias-project");
    if let Err(error) = create_dir_symlink(&real_root, &alias_root) {
        eprintln!("skipping symlink canonicalization test: {error}");
        return;
    }

    let canonical_root = fs::canonicalize(&real_root).expect("canonicalize real root");
    let canonical_file = fs::canonicalize(&source_file).expect("canonicalize source file");
    let alias_file = alias_root.join("src/lib.rs");
    let project_key = aft::search_index::artifact_cache_key(&canonical_root);
    aft::root_cache::configure_artifact_access(&canonical_root, &project_key, false);

    let mut search_index = SearchIndex::build(&canonical_root);
    assert!(
        search_index.path_to_id.contains_key(&canonical_file),
        "search index should store the canonical file key"
    );

    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|text| embedding_for(&text)).collect())
    };
    let mut semantic_index = SemanticIndex::build(
        &canonical_root,
        std::slice::from_ref(&canonical_file),
        &mut embed,
        64,
    )
    .expect("build semantic index");
    assert!(
        semantic_index.len() > 0,
        "semantic index should contain the canonical file entry"
    );

    fs::remove_file(&canonical_file).expect("delete canonical source file");
    assert!(
        !alias_file.exists(),
        "alias path should be missing after canonical delete"
    );

    search_index.remove_file(&alias_file);
    semantic_index.invalidate_file(&alias_file);

    assert!(
        !search_index.path_to_id.contains_key(&canonical_file),
        "deleted alias path should invalidate the canonical search-index key"
    );
    assert_eq!(
        semantic_index.len(),
        0,
        "deleted alias path should invalidate canonical semantic entries"
    );
}

#[test]
fn semantic_build_recovers_when_backend_returns_after_transient_outage() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    // Regression for the "Semantic Index: failed (won't recover)" report: a
    // transient backend outage during the initial build must NOT park the index
    // in `Failed` (a state nothing re-triggers short of a restart). The build
    // rides it out, showing a "waiting_for_embedding_backend" building stage,
    // and goes Ready the moment the backend returns.
    let project = setup_project(&[(
        "src/a.rs",
        "pub fn alpha_anchor() -> &'static str {\n    \"alpha anchor\"\n}\n",
    )]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let server = FlakyEmbeddingServer::start_down();
    // Shrink the retry backoff so the test doesn't wait the real 15s schedule.
    let mut aft = AftProcess::spawn_with_real_watcher_env(&[(
        "AFT_SEMANTIC_RETRY_BACKOFF_MS",
        std::ffi::OsStr::new("200"),
    )]);

    let configure =
        configure_semantic_openai(&mut aft, project.path(), storage.path(), &server.base_url);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    // While the backend is down, status must say so ("backend_unavailable",
    // with the reason and the next retry), not "loading" and never "failed":
    // the build is waiting on the backend, not broken.
    let waiting = wait_for_semantic_status(&mut aft, "waiting for backend", |response| {
        response["semantic_index"]["status"] == "backend_unavailable"
            && response["semantic_index"]["next_retry_ms"].is_u64()
    });
    assert_eq!(waiting["semantic_index"]["status"], "backend_unavailable");
    assert_eq!(
        waiting["semantic_index"]["backend_url"],
        server.base_url.as_str()
    );

    // Bring the backend up: the in-flight retry loop's next attempt succeeds.
    server.bring_up();

    let ready = wait_for_semantic_status(&mut aft, "recovered ready", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["files_embedded"]
                .as_u64()
                .unwrap_or(0)
                >= 1
    });
    assert_eq!(ready["semantic_index"]["status"], "ready");

    // And search actually works against the recovered index.
    let search = send(
        &mut aft,
        json!({
            "id": "recovered-search",
            "command": "semantic_search",
            "query": "alpha anchor",
            "hint": "semantic",
            "top_k": 5,
        }),
    );
    assert_eq!(
        search["success"], true,
        "semantic search should succeed after recovery: {search:?}"
    );
    assert_eq!(search["status"], "ready");

    let status = aft.shutdown();
    assert!(status.success());
}
