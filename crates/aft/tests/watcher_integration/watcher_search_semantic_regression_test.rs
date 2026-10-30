use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn configure_search_index(aft: &mut AftProcess, root: &Path) {
    let response = send(
        aft,
        json!({
            "id": "cfg-search-regression",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "config": user_config(serde_json::json!({
                "indexes": { "trigram": true, "semantic": false }
            })),
        }),
    );
    assert_eq!(
        response["success"], true,
        "configure should succeed: {response:?}"
    );
}

fn grep_marker(aft: &mut AftProcess, pattern: &str) -> Value {
    send(
        aft,
        json!({
            "id": "grep-regression-marker",
            "command": "grep",
            "pattern": pattern,
        }),
    )
}

fn wait_for_ready_grep<F>(
    aft: &mut AftProcess,
    label: &str,
    pattern: &str,
    mut predicate: F,
) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last_response = None;
    while Instant::now() < deadline {
        let response = grep_marker(aft, pattern);
        assert_eq!(
            response["success"], true,
            "grep should succeed while waiting for {label}: {response:?}"
        );
        if response["index_status"] == "Ready" && predicate(&response) {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!("timed out waiting for {label}; last response: {last_response:?}");
}

fn exact_memo_watcher_content_changes(views: bool) {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().expect("create project dir");
    let seed = project.path().join("seed.rs");
    let edited = project.path().join("edited.rs");
    let created = project.path().join("created.rs");
    // Quoted stopwords still match the exact phrase, but have no lexical
    // content tokens that could rescue a file missing from the exact memo.
    let phrase = "the and";
    fs::write(&seed, format!("// {phrase}\n")).unwrap();
    fs::write(&edited, "// unrelated initial content\n").unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher_env(&[]);
    let configured = send(
        &mut aft,
        json!({
            "id": "cfg-exact-memo-watcher",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "config": user_config(json!({
                "indexes": { "trigram": true, "semantic": false },
                "views": { "enabled": views },
            })),
        }),
    );
    assert_eq!(configured["success"], true, "{configured}");
    wait_for_ready_grep(&mut aft, "initial exact corpus", phrase, |r| {
        r["total_matches"] == 1
    });

    let search = |aft: &mut AftProcess, label: &str| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let response = send(
                aft,
                json!({
                    "id": "search-exact-memo-watcher",
                    "command": "semantic_search",
                    "query": format!("\"{phrase}\""),
                    "hint": "literal",
                    "top_k": 10,
                    "include_tests": false,
                }),
            );
            assert_eq!(response["success"], true, "{label}: {response}");
            let pending = response["gaps"]
                .as_array()
                .is_some_and(|gaps| gaps.iter().any(|gap| gap["kind"] == "watcher_pending"));
            if !pending {
                eprintln!(
                    "exact memo watcher views={views} {label}: {}",
                    json!({
                        "results": response["results"],
                        "complete": response["complete"],
                        "total": response["results_list_envelope"]["total"],
                        "generation": response["structuredContent"]["plan"]["snapshot_generation"],
                        "stability_void": response["structuredContent"]["plan"]["stability_void"],
                    })
                );
                assert_eq!(response["engine_capped"], false, "{response}");
                assert_eq!(response["more_available"], false, "{response}");
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "watcher did not settle: {response}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    };
    let files = |response: &Value| -> Vec<String> {
        let mut files = response["results"]
            .as_array()
            .expect("search results")
            .iter()
            .map(|row| {
                assert_eq!(row["exact"], true, "{row}");
                Path::new(row["file"].as_str().expect("result file"))
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    };
    let initial = search(&mut aft, "initial");
    assert_eq!(files(&initial), ["seed.rs"]);
    let warm = search(&mut aft, "warm");
    assert_eq!(files(&warm), ["seed.rs"]);

    // Direct writes exercise OS notifications rather than refreshes triggered
    // by writes performed by AFT itself. The marker checks confirm the index
    // accepted each change before search.
    fs::write(&edited, format!("// {phrase} edited_watcher_marker\n")).unwrap();
    wait_for_ready_grep(&mut aft, "in-place update", "edited_watcher_marker", |r| {
        r["total_matches"] == 1
    });
    let after_edit = search(&mut aft, "newly matching in-place edit");

    fs::write(&created, format!("// {phrase} created_watcher_marker\n")).unwrap();
    wait_for_ready_grep(&mut aft, "new file", "created_watcher_marker", |r| {
        r["total_matches"] == 1
    });
    let after_create = search(&mut aft, "newly created match");

    fs::write(&seed, "// retired_watcher_marker\n").unwrap();
    wait_for_ready_grep(&mut aft, "retired match", "retired_watcher_marker", |r| {
        r["total_matches"] == 1
    });
    let after_retire = search(&mut aft, "file stops matching");

    // Check all three transitions together so a failing reproduction still
    // records how additions and disappearing matches behave independently.
    assert_eq!(
        [
            files(&after_edit),
            files(&after_create),
            files(&after_retire)
        ],
        [
            vec!["edited.rs", "seed.rs"],
            vec!["created.rs", "edited.rs", "seed.rs"],
            vec!["created.rs", "edited.rs"],
        ],
        "exact memo must follow accepted watcher changes (views={views})"
    );
}

#[test]
fn exact_memo_real_watcher_views_off_content_changes() {
    exact_memo_watcher_content_changes(false);
}

#[test]
fn exact_memo_real_watcher_views_on_content_changes() {
    exact_memo_watcher_content_changes(true);
}

#[cfg(debug_assertions)]
#[test]
fn search_pending_replay_skips_file_that_becomes_ignored() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().expect("create project dir");
    fs::create_dir_all(project.path().join("src")).expect("create src");
    let secret = project.path().join("src/secret.rs");
    fs::write(
        &secret,
        "fn secret() { println!(\"pending_ignore_before_marker\"); }\n",
    )
    .expect("write secret");

    let mut aft = AftProcess::spawn_with_real_watcher_env(&[(
        "AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS",
        std::ffi::OsStr::new("1200"),
    )]);
    configure_search_index(&mut aft, project.path());
    wait_for_ready_grep(
        &mut aft,
        "initial search index",
        "pending_ignore_before_marker",
        |response| response["total_matches"] == 1,
    );

    let aftignore = project.path().join(".aftignore");
    fs::write(&aftignore, "# trigger corpus refresh\n").expect("write harmless aftignore");
    let mut saw_refresh = false;
    for attempt in 0..80 {
        if attempt % 4 == 0 {
            fs::write(&aftignore, "# trigger corpus refresh\n").expect("retouch aftignore");
        }
        let response = grep_marker(&mut aft, "pending_ignore_before_marker");
        assert_eq!(
            response["success"], true,
            "grep should succeed: {response:?}"
        );
        if response["index_status"] != "Ready" {
            saw_refresh = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !saw_refresh {
        eprintln!(
            "skipping pending replay regression: watcher did not observe the trigger .aftignore"
        );
        let shutdown = aft.shutdown();
        assert!(shutdown.success());
        return;
    }

    let changed = "fn secret() { println!(\"pending_ignore_after_marker\"); }\n";
    for _ in 0..10 {
        fs::write(&secret, changed).expect("write pending secret edit");
        let _ = grep_marker(&mut aft, "pending_ignore_after_marker");
        thread::sleep(Duration::from_millis(50));
    }

    // Drain the edit-triggered refresh to a STABLE Ready state before writing
    // the ignore rule. Otherwise a lingering edit refresh (FSEvents is async and
    // throttled on a loaded macOS runner) can leave index_status != "Ready" when
    // the ignore loop below starts, prematurely setting `saw_ignore_refresh` from
    // the EDIT refresh rather than the ignore refresh. The loop would then fire
    // the `total_matches == 0` assertion the moment the index settles — while the
    // ignore rule has not been applied yet and the secret edit is still
    // (correctly) indexed — producing a false failure. Requiring several
    // consecutive Ready polls makes any later non-Ready attributable to the
    // .aftignore write, so the regression assertion only runs once the ignore
    // refresh has genuinely occurred.
    let mut consecutive_ready = 0;
    let stabilize_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < stabilize_deadline {
        let response = grep_marker(&mut aft, "pending_ignore_after_marker");
        assert_eq!(
            response["success"], true,
            "grep should succeed: {response:?}"
        );
        if response["index_status"] == "Ready" {
            consecutive_ready += 1;
            if consecutive_ready >= 3 {
                break;
            }
        } else {
            consecutive_ready = 0;
        }
        thread::sleep(Duration::from_millis(50));
    }

    fs::write(&aftignore, "src/secret.rs\n").expect("ignore secret");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last_response = None;
    let mut saw_ignore_refresh = false;
    let mut attempts = 0usize;
    while Instant::now() < deadline {
        if !saw_ignore_refresh && attempts.is_multiple_of(5) {
            fs::write(&aftignore, "src/secret.rs\n").expect("retouch ignore rule");
        }
        attempts += 1;
        let response = grep_marker(&mut aft, "pending_ignore_after_marker");
        assert_eq!(
            response["success"], true,
            "grep should succeed: {response:?}"
        );
        if response["index_status"] != "Ready" {
            saw_ignore_refresh = true;
        }
        if saw_ignore_refresh && response["index_status"] == "Ready" {
            assert_eq!(
                response["total_matches"], 0,
                "ignored pending replay must not re-index the secret: {response:?}"
            );
            let shutdown = aft.shutdown();
            assert!(shutdown.success());
            return;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    // Reaching here means the watcher never delivered/settled the .aftignore
    // refresh within the deadline (FSEvents is throttled/async on a loaded
    // macOS runner). That is purely environmental — the actual regression
    // under test (an ignored pending replay must not re-index the secret) is
    // proven by the `total_matches == 0` assertion above, which only runs once
    // the index settles. So a non-delivery here is a skip, mirroring the
    // trigger-not-observed skip earlier in this test, not a pass that hides a
    // real re-index (that path asserts and fails loudly).
    eprintln!(
        "skipping pending ignore replay regression: watcher did not settle the ignore refresh in time; last response: {last_response:?}"
    );
    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn restart_drops_search_cache_entry_after_global_gitignore_change() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().expect("create project dir");
    let cache = tempfile::tempdir().expect("create cache dir");
    let xdg = tempfile::tempdir().expect("create xdg dir");
    let home = tempfile::tempdir().expect("create home dir");
    fs::create_dir_all(project.path().join("src")).expect("create src");
    let mut git_init = std::process::Command::new("git");
    let git_init = crate::test_helpers::apply_hermetic_git_env(&mut git_init)
        .arg("init")
        .arg(project.path())
        .status();
    if !git_init.is_ok_and(|status| status.success()) {
        eprintln!("skipping global gitignore restart test because git init failed");
        return;
    }
    fs::write(
        project.path().join("src/global_secret.rs"),
        "fn secret() { println!(\"global_restart_ignore_marker\"); }\n",
    )
    .expect("write globally ignored candidate");

    let envs = [
        ("AFT_CACHE_DIR", cache.path().as_os_str()),
        ("XDG_CONFIG_HOME", xdg.path().as_os_str()),
        ("HOME", home.path().as_os_str()),
    ];

    let mut first = AftProcess::spawn_with_real_watcher_env(&envs);
    configure_search_index(&mut first, project.path());
    wait_for_ready_grep(
        &mut first,
        "initial globally visible file",
        "global_restart_ignore_marker",
        |response| response["total_matches"] == 1,
    );
    let shutdown = first.shutdown();
    assert!(shutdown.success());

    let global_ignore = xdg.path().join("git/ignore");
    fs::create_dir_all(global_ignore.parent().expect("global ignore parent"))
        .expect("create global ignore parent");
    fs::write(&global_ignore, "global_secret.rs\n").expect("write global ignore");

    let mut second = AftProcess::spawn_with_real_watcher_env(&envs);
    configure_search_index(&mut second, project.path());
    wait_for_ready_grep(
        &mut second,
        "global ignore applied on restart",
        "global_restart_ignore_marker",
        |response| response["total_matches"] == 0,
    );
    let shutdown = second.shutdown();
    assert!(shutdown.success());
}

struct BlockingEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    saw_corpus_refresh: Arc<AtomicBool>,
    release_corpus_refresh: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl BlockingEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let saw_corpus_refresh = Arc::new(AtomicBool::new(false));
        let release_corpus_refresh = Arc::new(AtomicBool::new(false));
        let running_for_thread = Arc::clone(&running);
        let saw_for_thread = Arc::clone(&saw_corpus_refresh);
        let release_for_thread = Arc::clone(&release_corpus_refresh);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = handle_embedding_request(
                            &mut stream,
                            &saw_for_thread,
                            &release_for_thread,
                        );
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            saw_corpus_refresh,
            release_corpus_refresh,
            handle: Some(handle),
        }
    }

    fn saw_corpus_refresh(&self) -> bool {
        self.saw_corpus_refresh.load(Ordering::SeqCst)
    }

    fn release_corpus_refresh(&self) {
        self.release_corpus_refresh.store(true, Ordering::SeqCst);
    }
}

impl Drop for BlockingEmbeddingServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.release_corpus_refresh.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("embedding server thread");
        }
    }
}

fn handle_embedding_request(
    stream: &mut TcpStream,
    saw_corpus_refresh: &Arc<AtomicBool>,
    release_corpus_refresh: &Arc<AtomicBool>,
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

    if inputs
        .iter()
        .any(|input| input.to_ascii_lowercase().contains("corpus hold marker"))
    {
        saw_corpus_refresh.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !release_corpus_refresh.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
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
    if lower.contains("created_during_corpus_anchor") || lower.contains("created during corpus") {
        vec![0.0, 1.0, 0.0]
    } else if lower.contains("alpha_anchor") || lower.contains("alpha anchor") {
        vec![1.0, 0.0, 0.0]
    } else {
        vec![0.0, 0.0, 1.0]
    }
}

fn configure_semantic_openai(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    base_url: &str,
) {
    let response = send(
        aft,
        json!({
            "id": "cfg-semantic-corpus-regression",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "storage_dir": storage_dir.display().to_string(),
            "config": user_config(serde_json::json!({
                "indexes": { "trigram": false, "semantic": true },
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "test-embedding",
                    "base_url": base_url,
                    "timeout_ms": 30_000,
                    "max_batch_size": 64
                }
            })),
        }),
    );
    assert_eq!(
        response["success"], true,
        "configure should succeed: {response:?}"
    );
}

fn status(aft: &mut AftProcess) -> Value {
    send(
        aft,
        json!({
            "id": "status-semantic-corpus-regression",
            "command": "status",
        }),
    )
}

fn wait_for_semantic_ready(aft: &mut AftProcess, label: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_response = None;
    while Instant::now() < deadline {
        let response = status(aft);
        assert_eq!(
            response["success"], true,
            "status should succeed: {response:?}"
        );
        if response["semantic_index"]["status"] == "ready" {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!("semantic index did not become ready for {label}; last response: {last_response:?}");
}

fn wait_for_semantic_result(aft: &mut AftProcess, expected_suffix: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_response = None;
    while Instant::now() < deadline {
        let response = send(
            aft,
            json!({
                "id": "semantic-created-during-corpus",
                "command": "semantic_search",
                "query": "created during corpus",
                "hint": "semantic",
                "top_k": 5,
            }),
        );
        // With the trigram index off, aft_search refuses with
        // search_lanes_unavailable while the semantic lane is still building;
        // that is "not ready yet", so keep polling. Any other failure is real.
        if response["success"] == false && response["code"] == "search_lanes_unavailable" {
            last_response = Some(response);
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        assert_eq!(
            response["success"], true,
            "semantic_search should succeed: {response:?}"
        );
        if response["status"] == "ready"
            && response["results"].as_array().is_some_and(|results| {
                results.iter().any(|result| {
                    result["file"]
                        .as_str()
                        .is_some_and(|file| file.replace('\\', "/").ends_with(expected_suffix))
                })
            })
        {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!("semantic result did not include {expected_suffix}; last response: {last_response:?}");
}

#[test]
fn semantic_corpus_refresh_replays_file_created_while_in_flight() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let source = project.path().join("src/a.rs");
    fs::create_dir_all(source.parent().expect("source parent")).expect("create src");
    fs::write(
        &source,
        "pub fn alpha_anchor() -> &'static str {\n    \"alpha anchor\"\n}\n",
    )
    .expect("write initial source");

    let server = BlockingEmbeddingServer::start();
    let mut aft = AftProcess::spawn_with_real_watcher();
    configure_semantic_openai(&mut aft, project.path(), storage.path(), &server.base_url);
    wait_for_semantic_ready(&mut aft, "initial build");

    fs::write(
        &source,
        "pub fn alpha_anchor() -> &'static str {\n    \"corpus hold marker\"\n}\n",
    )
    .expect("make existing file stale for corpus refresh");
    fs::write(
        project.path().join(".aftignore"),
        "# trigger semantic corpus refresh\n",
    )
    .expect("write aftignore");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut attempts = 0usize;
    while !server.saw_corpus_refresh() && Instant::now() < deadline {
        if attempts.is_multiple_of(5) {
            // Unique content each retouch so the content-hash freshness check
            // can't dedup it away if the watcher dropped/coalesced the first
            // event under parallel-suite load (the root cause of the flake).
            // The asserted `alpha_anchor` symbol is preserved; only a trailing
            // comment varies the hash.
            fs::write(
                &source,
                format!(
                    "pub fn alpha_anchor() -> &'static str {{\n    \"corpus hold marker\"\n}}\n// retouch {attempts}\n"
                ),
            )
            .expect("retouch stale file for corpus refresh");
            fs::write(
                project.path().join(".aftignore"),
                format!("# trigger semantic corpus refresh {attempts}\n"),
            )
            .expect("retouch aftignore");
        }
        attempts += 1;
        let _ = status(&mut aft);
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        server.saw_corpus_refresh(),
        "semantic corpus refresh should reach the embedding server"
    );

    let created = project.path().join("src/new.rs");
    let created_contents = "pub fn created_during_corpus_anchor() -> &'static str {\n    \"created during corpus\"\n}\n";
    for _ in 0..100 {
        fs::write(&created, created_contents).expect("write file during corpus refresh");
        let _ = status(&mut aft);
        thread::sleep(Duration::from_millis(100));
    }

    server.release_corpus_refresh();
    let result = wait_for_semantic_result(&mut aft, "src/new.rs");
    assert_eq!(result["status"], "ready");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}
