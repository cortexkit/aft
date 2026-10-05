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
        "search_index": planes.trigram,
        "semantic_search": planes.semantic,
        "callgraph_store": planes.callgraph,
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
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    ));
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
            "child trigram index never persisted: {}",
            root.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn callgraph_view(root: &Path, storage: &Path) -> aft::views::ViewStore {
    aft::views::ViewStore::open(storage, &aft::path_identity::project_scope_key(root)).unwrap()
}

/// Waits until the child's content-addressed view publishes a generation
/// other than `previous`.
fn wait_callgraph_view(
    ctx: &AppContext,
    root: &Path,
    storage: &Path,
    previous: Option<&str>,
) -> String {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        aft::runtime_drain::drain_deferred_configure_maintenance(ctx);
        if let Some(generation) = callgraph_view(root, storage).current_generation().unwrap() {
            if Some(generation.as_str()) != previous {
                return generation;
            }
        }
        assert!(
            Instant::now() < deadline,
            "child call graph view never published: {}",
            root.display()
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

fn parent_session(ctx: &AppContext) -> Arc<aft::views::parent::ParentSession> {
    let session = aft::views::parent::session_for(ctx).expect("parent session active");
    assert!(
        session.wait_rounds(1, DEADLINE),
        "parent folder never finished its first load"
    );
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
    eprintln!(
        "parent folder grep timing (32 children, debug build): token in every child: parent median {parent_median:.2} ms, child median {child_median:.2} ms; needle in one child: parent median {parent_unique_median:.2} ms, child median {child_unique_median:.2} ms"
    );
    // Every child holds the common token, so the parent verifies 32 times the
    // files one child does; it must cost no more than that work.
    assert!(
        parent_median <= child_median * 32.0 + 100.0,
        "parent grep {parent_median:.2} ms is not in the order of a child grep {child_median:.2} ms"
    );
    assert!(
        parent_unique_median <= child_unique_median * 10.0 + 25.0,
        "parent grep {parent_unique_median:.2} ms is not in the order of a child grep {child_unique_median:.2} ms"
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

/// The call graph is served from a child's view while the child's SQLite
/// journal files exist, paths carry the child prefix, a child without a view
/// and a file outside every child are named gaps, and the generation the
/// parent serves survives the child's generation sweep.
#[test]
fn parent_callgraph_reads_child_views_and_protects_served_generation() {
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
    // The child session stays alive, so its view databases keep their
    // `-wal`/`-shm` journal files while the parent reads them.
    let child = configure(&graph, &storage, config_doc(true, with_graph, None));
    let first = wait_callgraph_view(&child, &graph, &storage, None);
    // `plain` is never bound to a session, so it has no view at all.
    let view_dir = callgraph_view(&graph, &storage).view_dir().to_path_buf();
    // Hold the derived database open in WAL mode, as a live child process
    // does: SQLite then keeps its `-wal` and `-shm` journal files beside it.
    let derived = std::fs::read_dir(&view_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("derived") && name.ends_with(".sqlite"))
        })
        .expect("published view has a derived database");
    let holder = rusqlite::Connection::open(&derived).unwrap();
    let mode: String = holder
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let _: i64 = holder
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    let journals = std::fs::read_dir(&view_dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.ends_with("-wal") || name.ends_with("-shm")
        })
        .count();
    assert!(journals > 0, "fixture must leave SQLite journal files");

    let parent = configure(&root, &storage, config_doc(true, with_graph, None));
    let session = parent_session(&parent);
    let graph_child = session
        .children()
        .iter()
        .find(|child| child.root == graph)
        .unwrap()
        .clone();
    assert!(
        graph_child.callgraph_ready(),
        "journal files must not lock the parent out"
    );
    assert_eq!(
        graph_child.callgraph_generation().as_deref(),
        Some(first.as_str())
    );

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

    // Hold the served generation, let the child publish a new one, and sweep
    // the child's view: the parent's read marker keeps the old generation.
    session.pause_refresh(true);
    drop(holder);
    drop(child);
    write(
        &graph,
        "src/lib.rs",
        "pub fn target() -> u32 {\n    2\n}\n\npub fn caller() -> u32 {\n    target()\n}\n",
    );
    commit_all(&graph, "second");
    let child = configure(&graph, &storage, config_doc(true, with_graph, None));
    let second = wait_callgraph_view(&child, &graph, &storage, Some(&first));
    let manifest = view_dir.join(format!("manifest-{first}.json"));
    // The child's own maintenance sweeps its view after publishing; the
    // generation the parent serves must survive that sweep and an explicit one.
    assert!(
        manifest.is_file(),
        "the child's post-publication sweep removed a generation the parent serves"
    );
    callgraph_view(&graph, &storage)
        .sweep_generations()
        .unwrap();
    assert!(
        manifest.is_file(),
        "a generation the parent serves must survive the child's sweep"
    );
    // Control: once the parent follows the child, the old generation is no
    // longer protected and the same sweep collects it.
    session.pause_refresh(false);
    let deadline = Instant::now() + DEADLINE;
    while graph_child.callgraph_generation().as_deref() != Some(second.as_str()) {
        assert!(Instant::now() < deadline, "parent never followed the child");
        thread::sleep(Duration::from_millis(20));
    }
    callgraph_view(&graph, &storage)
        .sweep_generations()
        .unwrap();
    assert!(!manifest.exists(), "an unprotected old generation is swept");
    drop(child);
}

/// `aft_search` merges the children's semantic views by score under the
/// parent prefix, even when a child also has a legacy semantic artifact larger
/// than the borrowed reader's size cap: the parent reads the child's
/// per-checkout view, which has no such cap.
#[test]
fn parent_search_merges_child_semantic_views_without_a_size_cap() {
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
    for child in [&alpha, &beta] {
        let ctx = configure(child, &storage, config_doc(true, semantic, Some(&embedder)));
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

    let parent = configure(&root, &storage, config_doc(true, semantic, Some(&embedder)));
    let session = parent_session(&parent);
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
    precomputed_query_vector_ranks_byte_identically(&sessions[0], "retry with exponential backoff");
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
/// a walk slowed to seconds, the bind still answers promptly, queries name
/// the discovery as a gap meanwhile, and the children appear once it ends.
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
    // 150 ms per examined entry: the full walk takes about 3 s, while the
    // configure probe stops at the first repository it meets.
    aft::views::parent::set_discovery_entry_delay(&root, Duration::from_millis(150));
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    ));
    let started = Instant::now();
    let configured = aft::commands::configure::handle_configure(
        &request(json!({
            "id": "configure-slow-parent",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(config_doc(true, TRIGRAM_ONLY, None)),
        })),
        &ctx,
    );
    let bind = started.elapsed();
    assert!(configured.success, "{configured:?}");
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let first = grep(&ctx, "anything");
    let session = aft::views::parent::session_for(&ctx).expect("parent session");
    let discovered_at = Instant::now() + DEADLINE;
    assert!(
        gap_kinds(&first).contains(&"parent_discovering".to_string()),
        "{first:#}"
    );
    eprintln!(
        "parent folder bind with a 3 s discovery walk answered in {} ms",
        bind.as_millis()
    );
    assert!(
        bind < Duration::from_millis(2_000),
        "the bind waited for discovery: {} ms",
        bind.as_millis()
    );
    while session.discovery().is_none() {
        assert!(Instant::now() < discovered_at, "discovery never finished");
        thread::sleep(Duration::from_millis(20));
    }
    aft::views::parent::set_discovery_entry_delay(&root, Duration::ZERO);
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

/// The standalone (stdin/stdout) runtime keeps answering requests while the
/// first views-on publication of a checkout runs: the publication detaches
/// from the request thread, as it does under the daemon, and still commits
/// while requests keep arriving. Before, a large checkout's first publication
/// ran inline on the request thread and every request waited minutes.
#[test]
fn standalone_view_publication_never_blocks_requests() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("repo");
    let storage = base.join("storage");
    init_repo(&root, &child_files(0));
    let delay = Duration::from_secs(20);
    let delay_ms = delay.as_millis().to_string();
    let mut aft = crate::helpers::AftProcess::spawn_with_env(&[(
        "AFT_TEST_VIEW_PUBLICATION_DELAY_MS",
        std::ffi::OsStr::new(&delay_ms),
    )]);
    let configured = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(config_doc(true, TRIGRAM_ONLY, None)),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:#}");
    let started = Instant::now();
    let grep_request = json!({"id": "grep", "command": "grep", "pattern": COMMON}).to_string();
    let mut slowest = Duration::ZERO;
    // Requests during the publication: each must be answered promptly.
    while started.elapsed() < Duration::from_secs(6) {
        let sent = Instant::now();
        let answer = aft.send_with_timeout(&grep_request, delay / 2);
        slowest = slowest.max(sent.elapsed());
        assert_eq!(answer["success"], true, "{answer:#}");
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "standalone requests during a {} s view publication: slowest {} ms",
        delay.as_secs(),
        slowest.as_millis()
    );
    assert!(
        slowest < Duration::from_secs(5),
        "a request waited {} ms for the view publication",
        slowest.as_millis()
    );
    // The detached publication still commits while requests keep arriving.
    let view = aft::views::ViewStore::open(&storage, &aft::path_identity::project_scope_key(&root))
        .unwrap();
    let deadline = Instant::now() + delay + DEADLINE;
    while view.current_generation().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the view publication never committed"
        );
        let answer = aft.send(&grep_request);
        assert_eq!(answer["success"], true, "{answer:#}");
        thread::sleep(Duration::from_millis(200));
    }
    assert!(aft.shutdown().success());
}

/// A view generation published with the call graph off holds no call graph.
/// Every reader of a view's call graph reports it unavailable, by name, never
/// as zero callers: here the call graph tools (and zoom's call graph field)
/// in a standalone process whose current generation was published with the
/// call graph off, while the republish that builds the graph is still
/// running. Once that republish lands, the same query answers.
#[test]
fn callgraph_ops_report_a_keyless_view_generation_as_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("repo");
    let storage = base.join("storage");
    init_repo(
        &root,
        &[(
            "src/lib.rs",
            "pub fn target() -> u32 {\n    1\n}\n\npub fn caller() -> u32 {\n    target()\n}\n"
                .to_string(),
        )],
    );
    publish_without_callgraph(&root, &storage);
    let mut aft = crate::helpers::AftProcess::spawn_with_env(&[(
        "AFT_TEST_VIEW_PUBLICATION_DELAY_MS",
        std::ffi::OsStr::new("8000"),
    )]);
    let with_graph = Planes {
        trigram: true,
        callgraph: true,
        semantic: false,
    };
    let configured = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(config_doc(true, with_graph, None)),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:#}");
    // Configure finishes its view setup in background steps, one after each
    // request; send a few requests so it loads the current generation (the
    // one without call graph data) and starts republishing with the graph.
    for _ in 0..40 {
        aft.send(&json!({"id": "ping", "command": "ping"}).to_string());
        thread::sleep(Duration::from_millis(20));
    }
    let callers = json!({
        "id": "callers",
        "command": "callers",
        "file": root.join("src/lib.rs"),
        "symbol": "target",
    })
    .to_string();
    let answer = aft.send(&callers);
    assert_eq!(answer["success"], false, "{answer:#}");
    assert_eq!(answer["code"], "callgraph_unavailable", "{answer:#}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap()
            .contains("call graph is disabled (indexes.callgraph=false)"),
        "{answer:#}"
    );
    let zoom = aft.send(
        &json!({
            "id": "zoom",
            "command": "zoom",
            "file": root.join("src/lib.rs"),
            "symbol": "target",
            "callgraph": true,
        })
        .to_string(),
    );
    // Zoom's own call lists come from the file's syntax tree; its call graph
    // field must say the index is not serving, not present an empty graph.
    assert_eq!(zoom["callgraph"]["status"], "unavailable", "{zoom:#}");
    assert!(
        zoom["callgraph"]["index"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("call graph is disabled (indexes.callgraph=false)"),
        "{zoom:#}"
    );
    // Once the republish with call graph data lands, callers answers. Right
    // after that publication its derived database is checkpointed, and a
    // reader that meets the checkpoint gets a retryable `callgraph_building`.
    // That answer is acceptable only once the published generation carries a
    // call graph: the pointer is read before each request, so "building"
    // while the generation without call graph data is still current fails.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let keyed_generation_published = current_generation_has_callgraph(&root, &storage);
        let answer = aft.send(&callers);
        if answer["success"] == true {
            assert!(answer.to_string().contains("caller"), "{answer:#}");
            break;
        }
        if !(keyed_generation_published && answer["code"] == "callgraph_building") {
            assert_eq!(answer["code"], "callgraph_unavailable", "{answer:#}");
        }
        assert!(
            Instant::now() < deadline,
            "the call graph was never published"
        );
        thread::sleep(Duration::from_millis(250));
    }
    let zoom = aft.send(
        &json!({
            "id": "zoom-ready",
            "command": "zoom",
            "file": root.join("src/lib.rs"),
            "symbol": "target",
            "callgraph": true,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "{zoom:#}");
    assert_ne!(zoom["callgraph"]["status"], "unavailable", "{zoom:#}");
    assert!(aft.shutdown().success());
}

/// Whether `root`'s current published view generation carries call graph
/// data. False while no generation is published or its manifest cannot be
/// read (a superseded generation may be collected between the two reads).
fn current_generation_has_callgraph(root: &Path, storage: &Path) -> bool {
    let view =
        aft::views::ViewStore::open(storage, &aft::path_identity::project_scope_key(root)).unwrap();
    let Some(generation) = view.current_generation_read_only().unwrap() else {
        return false;
    };
    view.load_manifest(&generation)
        .is_ok_and(|manifest| !aft::views::assembly::manifest_lacks_callgraph(&manifest))
}

/// Publishes `root`'s current view generation with the call graph off, as a
/// views-on session with `callgraph_store: false` does.
fn publish_without_callgraph(root: &Path, storage: &Path) {
    let head = aft::alias::head_tree_entries(root).unwrap();
    let report = aft::views::assembly::publish_checkout(&aft::views::assembly::AssemblyRequest {
        storage: storage.to_path_buf(),
        project_root: root.to_path_buf(),
        family: aft::search_index::artifact_cache_key(root),
        scope: aft::path_identity::project_scope_key(root),
        desired_head: aft::views::assembly::head_tree_fingerprint(&head),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: false,
    })
    .unwrap();
    assert!(report.published);
    assert!(aft::views::assembly::manifest_lacks_callgraph(
        report.manifest.as_ref().unwrap()
    ));
}

/// A parent folder names a child whose view was published with the call
/// graph off as unavailable for the call graph, never as having no callers.
#[test]
fn parent_names_a_child_view_without_callgraph_as_disabled() {
    fast_refresh();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    let child = root.join("child");
    init_repo(&child, &[("src/lib.rs", "pub fn alone() {}\n".to_string())]);
    publish_without_callgraph(&child, &storage);
    let with_graph = Planes {
        trigram: true,
        callgraph: true,
        semantic: false,
    };
    let parent = configure(&root, &storage, config_doc(true, with_graph, None));
    parent_session(&parent);
    let answer = aft::views::parent::route(
        &request(json!({
            "id": "parent-callers",
            "command": "callers",
            "file": "child/src/lib.rs",
            "symbol": "alone",
        })),
        &parent,
    )
    .unwrap();
    assert!(!answer.success, "{:#}", answer.data);
    assert_eq!(answer.data["complete"], false);
    assert_eq!(answer.data["gaps"][0]["path"], "child");
    assert!(
        answer.data["gaps"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("call graph is disabled (indexes.callgraph=false)"),
        "{:#}",
        answer.data
    );
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
/// serves the child from the index the child's own session persisted,
/// without the child ever being bound in the new process, and without
/// writing the child's artifact.
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
    let deadline = Instant::now() + DEADLINE;
    while !artifact.is_file() {
        assert!(Instant::now() < deadline, "child index never persisted");
        first.send(&grep_request);
        thread::sleep(Duration::from_millis(100));
    }
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
    let before = std::fs::read(&artifact).unwrap();

    let mut second = crate::helpers::AftProcess::spawn();
    let configured = second.send(&configure_request(&root));
    assert_eq!(configured["success"], true, "{configured:#}");
    let answer = wait_for_match(&mut second);
    assert_eq!(answer["complete"], true, "{answer:#}");
    assert!(second.shutdown().success());
    assert_eq!(std::fs::read(&artifact).unwrap(), before);
}
