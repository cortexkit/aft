use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

const ROW_DEADLINE: Duration = Duration::from_secs(60);

struct MockEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    batches: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server address");
        let running = Arc::new(AtomicBool::new(true));
        let batches = Arc::new(AtomicUsize::new(0));
        let running_for_thread = Arc::clone(&running);
        let batches_for_thread = Arc::clone(&batches);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let batches = Arc::clone(&batches_for_thread);
                        thread::spawn(move || {
                            let _ = handle_embedding_request(&mut stream, &batches);
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
            batches,
            handle: Some(handle),
        }
    }

    fn batch_count(&self) -> usize {
        self.batches.load(Ordering::SeqCst)
    }
}

impl Drop for MockEmbeddingServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("embedding server thread");
        }
    }
}

fn handle_embedding_request(stream: &mut TcpStream, batches: &AtomicUsize) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = Some(position + 4);
                for line in String::from_utf8_lossy(&bytes[..position + 4]).lines() {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }
    let body = header_end
        .and_then(|end| bytes.get(end..end + content_length))
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
    if inputs
        .iter()
        .any(|input| input != "semantic index fingerprint probe")
    {
        batches.fetch_add(1, Ordering::SeqCst);
    }
    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let hash = blake3::hash(input.as_bytes());
            json!({
                "embedding": [
                    f32::from(hash.as_bytes()[0]) / 255.0,
                    f32::from(hash.as_bytes()[1]) / 255.0,
                    f32::from(hash.as_bytes()[2]) / 255.0
                ],
                "index": index
            })
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

struct RepoFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    changed_paths: Vec<PathBuf>,
}

impl RepoFixture {
    fn new(file_count: usize, changed_count: usize) -> Self {
        assert!(file_count >= changed_count.max(3));
        let temp = tempfile::tempdir().expect("create repository tempdir");
        let root = std::fs::canonicalize(temp.path()).expect("canonical repository root");
        git(&root, &["init", "-q"]);
        git(
            &root,
            &["config", "user.email", "branch-switch@example.test"],
        );
        git(&root, &["config", "user.name", "Branch Switch Test"]);
        for index in 0..file_count {
            write_branch_file(&root, index, 'A');
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "branch A"]);
        git(&root, &["branch", "-M", "A"]);
        git(&root, &["checkout", "-qb", "B"]);
        for index in 0..changed_count {
            write_branch_file(&root, index, 'B');
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "branch B"]);
        git(&root, &["checkout", "-q", "A"]);
        let changed_paths = (0..changed_count)
            .map(|index| root.join(format!("src/file-{index:03}.ts")))
            .collect();
        Self {
            _temp: temp,
            root,
            changed_paths,
        }
    }
}

fn write_branch_file(root: &Path, index: usize, branch: char) {
    let path = root.join(format!("src/file-{index:03}.ts"));
    std::fs::create_dir_all(path.parent().expect("source parent")).unwrap();
    let source = match index {
        0 => format!("export function target{branch}() {{ return '{branch}'; }}\n"),
        1 => format!(
            "import {{ target{branch} }} from './file-000';\nexport function caller{branch}() {{ return target{branch}(); }}\n"
        ),
        2 => format!("export function dead{branch}() {{ return 'dead-{branch}'; }}\n"),
        _ => format!("export function branch{branch}File{index}() {{ return {index}; }}\n"),
    };
    std::fs::write(path, source).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).expect("valid request")
}

fn response(response: Response) -> Value {
    serde_json::to_value(response).expect("response serializes")
}

fn configure_context(
    root: &Path,
    storage: &Path,
    server: &MockEmbeddingServer,
    ram_overlay: bool,
) -> Arc<AppContext> {
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    let configure = request(json!({
        "id": "configure-branch-switch",
        "command": "configure",
        "harness": "opencode",
        "project_root": root,
        "storage_dir": storage,
        "config": crate::helpers::user_config(json!({
            "search_index": true,
            "semantic_search": true,
            "callgraph_store": true,
            "worktree": { "ram_overlay": ram_overlay },
            "semantic": {
                "backend": "openai_compatible",
                "model": "branch-switch-mock",
                "base_url": server.base_url,
                "timeout_ms": 5_000,
                "max_batch_size": 1_000,
                "max_files": 2_000
            }
        }))
    }));
    let configured = aft::commands::configure::handle_configure(&configure, &ctx);
    assert!(configured.success, "configure failed: {configured:?}");
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    wait_until_ready(&ctx);
    ctx
}

fn wait_until_ready(ctx: &AppContext) -> Arc<aft::callgraph_store::ReadonlyCallGraphStore> {
    let deadline = Instant::now() + ROW_DEADLINE;
    loop {
        aft::runtime_drain::drain_watcher_events(ctx);
        aft::runtime_drain::drain_search_index_events(ctx);
        aft::runtime_drain::drain_callgraph_store_events(ctx);
        aft::runtime_drain::drain_semantic_index_events(ctx);
        aft::runtime_drain::drain_semantic_refresh_events(ctx);
        let callgraph = match ctx.callgraph_store_for_ops() {
            CallgraphStoreAccess::Ready(store) => Some(store),
            CallgraphStoreAccess::Building => None,
            CallgraphStoreAccess::Suspended(reason) => {
                panic!("callgraph suspended while waiting: {reason:?}")
            }
            CallgraphStoreAccess::Unavailable => panic!("callgraph unavailable while waiting"),
            CallgraphStoreAccess::Error(error) => panic!("callgraph failed while waiting: {error}"),
        };
        let search_ready = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|index| index.ready);
        let semantic_ready = matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
        );
        if let Some(store) = callgraph.filter(|_| search_ready && semantic_ready) {
            return store;
        }
        assert!(Instant::now() < deadline, "indexes did not become ready");
        thread::sleep(Duration::from_millis(10));
    }
}

fn grep(ctx: &AppContext, pattern: &str) -> Value {
    response(aft::commands::grep::handle_grep(
        &request(json!({
            "id": "branch-switch-grep",
            "command": "grep",
            "pattern": pattern,
            "path": ".",
            "limit": 20
        })),
        ctx,
    ))
}

fn callers(ctx: &AppContext, root: &Path, branch: char) -> Value {
    response(aft::commands::callers::handle_callers(
        &request(json!({
            "id": "branch-switch-callers",
            "command": "callers",
            "file": root.join("src/file-000.ts"),
            "symbol": format!("target{branch}"),
            "depth": 1
        })),
        ctx,
    ))
}

fn projected_exports(store: &aft::callgraph_store::ReadonlyCallGraphStore) -> BTreeSet<String> {
    aft::callgraph_store::project_dead_code_snapshot(store.sqlite_path())
        .expect("project dead-code snapshot")
        .exported_symbols
        .into_iter()
        .map(|export| export.symbol)
        .collect()
}

fn assert_branch_state(ctx: &AppContext, root: &Path, branch: char) -> BranchSnapshot {
    let other = if branch == 'A' { 'B' } else { 'A' };
    let deadline = Instant::now() + ROW_DEADLINE;
    loop {
        let store = wait_until_ready(ctx);
        let present = grep(ctx, &format!("target{branch}"));
        let absent = grep(ctx, &format!("target{other}"));
        let caller_result = callers(ctx, root, branch);
        let exports = projected_exports(&store);
        let ready = present["index_status"] == "Ready"
            && present["total_matches"].as_u64().unwrap_or(0) > 0
            && absent["total_matches"] == 0
            && caller_result["success"] == true
            && caller_result["total_callers"].as_u64().unwrap_or(0) > 0
            && exports.contains(&format!("dead{branch}"))
            && !exports.contains(&format!("dead{other}"));
        if ready {
            return BranchSnapshot {
                search_matches: order_insensitive_matches(&present["matches"]),
                callers: caller_result["callers"].clone(),
                projected_exports: exports,
            };
        }
        assert!(
            Instant::now() < deadline,
            "branch {branch} did not converge: present={present:#} absent={absent:#} callers={caller_result:#} exports={exports:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// grep orders matches by file mtime, and a branch round trip rewrites the
/// same files in a different order, so two projections of the same state can
/// legitimately differ in match order. Compare them by content instead.
fn order_insensitive_matches(matches: &Value) -> Value {
    let mut sorted: Vec<Value> = matches.as_array().cloned().unwrap_or_default();
    sorted.sort_by_key(|entry| {
        (
            entry["file"].as_str().unwrap_or("").to_string(),
            entry["line"].as_u64().unwrap_or(0),
            entry["column"].as_u64().unwrap_or(0),
        )
    });
    Value::Array(sorted)
}

#[derive(Debug, PartialEq, Eq)]
struct BranchSnapshot {
    search_matches: Value,
    callers: Value,
    projected_exports: BTreeSet<String>,
}

fn assert_no_cold_rebuild(
    ctx: &AppContext,
    resident: &Arc<aft::callgraph_store::ReadonlyCallGraphStore>,
) {
    let installed = ctx
        .callgraph_store()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(Arc::clone)
        .expect("callgraph store remains resident");
    assert!(
        Arc::ptr_eq(resident, &installed),
        "branch switch must retain the resident store generation"
    );
    assert_eq!(
        ctx.pending_callgraph_store_force_token_for_test(),
        None,
        "branch switch paths must not mint a corpus-drift force token"
    );
}

// The per-row deadline is load-bearing: a regression to a cold callgraph rebuild
// can remain functionally correct but cannot finish these fixture transitions in time.
fn run_row(label: &str, row: impl FnOnce()) {
    let started = Instant::now();
    row();
    assert!(
        started.elapsed() < ROW_DEADLINE,
        "branch-switch row {label} exceeded the 60s wall-clock bound"
    );
}

#[test]
fn branch_switch_e2e_matrix() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let previous_disable = std::env::var_os("AFT_TEST_DISABLE_FILE_WATCHER");
    let previous_sync = std::env::var_os("AFT_TEST_SYNC_FILE_WATCHER_START");
    // SAFETY: these process-global variables are changed only while watcher_serial_lock
    // prevents every other real-watcher test from reading or changing them concurrently.
    unsafe {
        std::env::set_var("AFT_TEST_DISABLE_FILE_WATCHER", "0");
        std::env::set_var("AFT_TEST_SYNC_FILE_WATCHER_START", "1");
    }
    let result = std::panic::catch_unwind(|| {
        let server = MockEmbeddingServer::start();

        run_row("small diff", || {
            let repo = RepoFixture::new(40, 20);
            let storage = tempfile::tempdir().unwrap();
            let ctx = configure_context(&repo.root, storage.path(), &server, false);
            let resident = wait_until_ready(&ctx);
            git(&repo.root, &["checkout", "-q", "B"]);
            assert_branch_state(&ctx, &repo.root, 'B');
            // After switching to B, stale search state could still return branch A's target
            // instead of the target defined by branch B.
            assert_no_cold_rebuild(&ctx, &resident);
        });

        run_row("large diff cliff", || {
            let repo = RepoFixture::new(400, 300);
            let storage = tempfile::tempdir().unwrap();
            let ctx = configure_context(&repo.root, storage.path(), &server, false);
            let resident = wait_until_ready(&ctx);
            git(&repo.root, &["checkout", "-q", "B"]);
            let (tx, rx) = crossbeam_channel::unbounded();
            *ctx.watcher_rx().lock() = Some(rx);
            tx.send(WatcherDispatchEvent::Paths(repo.changed_paths.clone()))
                .unwrap();
            assert_branch_state(&ctx, &repo.root, 'B');
            // A switch changing more than 256 paths must retain this resident Arc;
            // dropping it would force the callgraph to rebuild from the full corpus.
            assert_no_cold_rebuild(&ctx, &resident);
        });

        run_row("round trip", || {
            let repo = RepoFixture::new(40, 20);
            let storage = tempfile::tempdir().unwrap();
            let ctx = configure_context(&repo.root, storage.path(), &server, false);
            let original = assert_branch_state(&ctx, &repo.root, 'A');
            git(&repo.root, &["checkout", "-q", "B"]);
            assert_branch_state(&ctx, &repo.root, 'B');
            let before_return = server.batch_count();
            git(&repo.root, &["checkout", "-q", "A"]);
            let returned = assert_branch_state(&ctx, &repo.root, 'A');
            // Search, callers, and dead-code projections must return exactly to their original A state.
            assert_eq!(
                returned, original,
                "A -> B -> A must restore exact projections"
            );
            let return_batches = server.batch_count().saturating_sub(before_return);
            assert_eq!(
                return_batches, 1,
                "current return switch embeds one coalesced batch"
            );
            // TODO: once content-addressed semantic storage reuses A's embeddings on the
            // return switch, replace the assertion above with assert_eq!(return_batches, 0).
        });

        run_row("rebase", || {
            let repo = RepoFixture::new(40, 20);
            git(&repo.root, &["checkout", "-q", "B"]);
            let storage = tempfile::tempdir().unwrap();
            let ctx = configure_context(&repo.root, storage.path(), &server, false);
            let resident = wait_until_ready(&ctx);
            git(&repo.root, &["checkout", "-q", "A"]);
            std::fs::write(
                repo.root.join("src/moved-base.ts"),
                "export function movedBaseA() { return true; }\n",
            )
            .unwrap();
            git(&repo.root, &["add", "."]);
            git(&repo.root, &["commit", "-qm", "move A"]);
            git(&repo.root, &["checkout", "-q", "B"]);
            git(&repo.root, &["rebase", "A"]);
            assert_branch_state(&ctx, &repo.root, 'B');
            // A rebase can add, remove, or change files in both directions; callers must
            // remain valid without rebuilding the callgraph from the full corpus.
            assert_no_cold_rebuild(&ctx, &resident);
        });

        run_row("stash pop", || {
            let repo = RepoFixture::new(80, 20);
            let storage = tempfile::tempdir().unwrap();
            let ctx = configure_context(&repo.root, storage.path(), &server, false);
            for index in 20..70 {
                std::fs::write(
                    repo.root.join(format!("src/file-{index:03}.ts")),
                    format!("export function dirtyPop{index}() {{ return {index}; }}\n"),
                )
                .unwrap();
            }
            git(&repo.root, &["stash", "push", "-qm", "dirty set"]);
            git(&repo.root, &["checkout", "-q", "B"]);
            git(&repo.root, &["stash", "pop", "-q"]);
            let deadline = Instant::now() + ROW_DEADLINE;
            loop {
                wait_until_ready(&ctx);
                let dirty = grep(&ctx, "dirtyPop69");
                if dirty["index_status"] == "Ready" && dirty["total_matches"] == 1 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "stash-pop edits did not reach search"
                );
                thread::sleep(Duration::from_millis(25));
            }
            // All 50 dirty edits must reappear after pop, including the final `dirtyPop69`
            // marker; otherwise the incremental update is incomplete.
        });

        run_row("linked worktree overlay", || {
            let repo = RepoFixture::new(40, 20);
            let storage = tempfile::tempdir().unwrap();
            let parent = configure_context(&repo.root, storage.path(), &server, false);
            let parent_store = wait_until_ready(&parent);
            let worktree_parent = tempfile::tempdir().unwrap();
            let worktree = worktree_parent.path().join("branch-b");
            git(
                &repo.root,
                &["worktree", "add", "-q", worktree.to_str().unwrap(), "B"],
            );
            let borrower = configure_context(&worktree, storage.path(), &server, true);
            std::fs::write(
                worktree.join("src/overlay-only.ts"),
                "export function overlayOnlyB() { return 'overlay'; }\n",
            )
            .unwrap();
            let deadline = Instant::now() + ROW_DEADLINE;
            loop {
                wait_until_ready(&borrower);
                let overlay = grep(&borrower, "overlayOnlyB");
                if overlay["index_status"] == "Ready" && overlay["total_matches"] == 1 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "worktree RAM overlay did not expose edit"
                );
                thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(grep(&parent, "overlayOnlyB")["total_matches"], 0);
            // The linked worktree must keep writes in its RAM overlay; writing the parent's
            // shared artifact would replace the parent's resident store and force a rebuild.
            assert_no_cold_rebuild(&parent, &parent_store);
        });
    });
    unsafe {
        match previous_disable {
            Some(value) => std::env::set_var("AFT_TEST_DISABLE_FILE_WATCHER", value),
            None => std::env::remove_var("AFT_TEST_DISABLE_FILE_WATCHER"),
        }
        match previous_sync {
            Some(value) => std::env::set_var("AFT_TEST_SYNC_FILE_WATCHER_START", value),
            None => std::env::remove_var("AFT_TEST_SYNC_FILE_WATCHER_START"),
        }
    }
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
