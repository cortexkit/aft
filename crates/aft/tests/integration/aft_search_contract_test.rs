use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::grep::handle_grep;
use aft::commands::semantic_search::handle_semantic_search;
use aft::config::{Config, SemanticBackend, SemanticBackendConfig};
use aft::context::{AppContext, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_index::{artifact_cache_key, resolve_cache_dir, SearchIndex};
use aft::semantic_index::{SemanticIndex, SemanticIndexFingerprint};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn request(query: &str) -> RawRequest {
    request_with(query, None)
}

fn request_with(query: &str, hint: Option<&str>) -> RawRequest {
    request_with_top_k(query, hint, 5)
}

fn request_with_top_k(query: &str, hint: Option<&str>, top_k: usize) -> RawRequest {
    let mut value = serde_json::json!({
        "id": "aft-search-contract",
        "command": "semantic_search",
        "query": query,
        "top_k": top_k,
    });
    if let Some(hint) = hint {
        value["hint"] = serde_json::json!(hint);
    }
    serde_json::from_value(value).expect("build semantic search request")
}

fn request_with_path(query: &str, hint: Option<&str>, path: &Path) -> RawRequest {
    let mut value = serde_json::json!({
        "id": "aft-search-contract",
        "command": "semantic_search",
        "query": query,
        "top_k": 5,
        "path": path.display().to_string(),
    });
    if let Some(hint) = hint {
        value["hint"] = serde_json::json!(hint);
    }
    serde_json::from_value(value).expect("build semantic search request with path")
}

fn grep_request(pattern: &str, max_results: usize) -> RawRequest {
    serde_json::from_value(serde_json::json!({
        "id": "grep-contract",
        "command": "grep",
        "pattern": pattern,
        "max_results": max_results,
    }))
    .expect("build grep request")
}

fn request_with_include_tests(
    query: &str,
    hint: Option<&str>,
    top_k: usize,
    include_tests: bool,
) -> RawRequest {
    let mut value = serde_json::json!({
        "id": "aft-search-contract",
        "command": "semantic_search",
        "query": query,
        "top_k": top_k,
        "include_tests": include_tests,
    });
    if let Some(hint) = hint {
        value["hint"] = serde_json::json!(hint);
    }
    serde_json::from_value(value).expect("build semantic search request")
}

fn response_value(response: Response) -> Value {
    serde_json::to_value(response).expect("serialize response")
}

fn test_context(project_root: &Path) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(project_root.to_path_buf()),
            ..Config::default()
        }),
    )
}

fn test_context_with_storage(project_root: &Path, storage_dir: &Path) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(project_root.to_path_buf()),
            storage_dir: Some(storage_dir.to_path_buf()),
            ..Config::default()
        }),
    )
}

fn git_command(root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    command
}

fn init_git(root: &Path) {
    let status = git_command(root).args(["init"]).status().expect("git init");
    assert!(status.success(), "git init failed");
    for (key, value) in [
        ("user.email", "test@example.com"),
        ("user.name", "AFT Test"),
    ] {
        let status = git_command(root)
            .args(["config", key, value])
            .status()
            .expect("git config");
        assert!(status.success(), "git config {key} failed");
    }
}

fn commit_all(root: &Path) {
    let status = git_command(root)
        .args(["add", "."])
        .status()
        .expect("git add");
    assert!(status.success(), "git add failed");
    let status = git_command(root)
        .args(["commit", "--no-gpg-sign", "-m", "initial"])
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");
}

fn git_project_with_needle() -> (tempfile::TempDir, std::path::PathBuf, &'static str) {
    let (project, source_file, source) = project_with_needle();
    init_git(project.path());
    commit_all(project.path());
    (project, source_file, source)
}

#[test]
fn artifact_cache_key_uses_sorted_roots_for_grafted_history() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let dir = tempfile::tempdir().expect("create grafted-history temp dir");
    let repo = dir.path().join("grafted-repo");
    fs::create_dir_all(&repo).expect("create grafted repo");
    init_git(&repo);

    fs::write(repo.join("first.txt"), "first root\n").expect("write first root file");
    commit_all(&repo);
    let first_root = git_head(&repo);

    let orphan = git_command(&repo)
        .args(["checkout", "--orphan", "second-root"])
        .status()
        .expect("create orphan branch");
    assert!(orphan.success(), "orphan branch creation failed");
    let remove_first = git_command(&repo)
        .args(["rm", "-rf", "."])
        .status()
        .expect("remove first root files");
    assert!(remove_first.success(), "removing first root files failed");
    fs::write(repo.join("second.txt"), "second root\n").expect("write second root file");
    commit_all(&repo);
    let second_root = git_head(&repo);

    let restore_first = git_command(&repo)
        .args(["checkout", "-b", "grafted-base", &first_root])
        .status()
        .expect("restore first root branch");
    assert!(
        restore_first.success(),
        "restoring first root branch failed"
    );
    let merge = git_command(&repo)
        .args([
            "merge",
            "--allow-unrelated-histories",
            "--no-edit",
            "-m",
            "graft roots",
            "second-root",
        ])
        .status()
        .expect("merge unrelated roots");
    assert!(merge.success(), "grafted-history merge failed");

    let mut roots = [first_root, second_root];
    roots.sort_unstable();
    let canonical_roots = roots.join("\n");
    let digest = format!("{:x}", Sha256::digest(canonical_roots.as_bytes()));
    let expected_key = digest[..16].to_string();

    assert_eq!(
        artifact_cache_key(&repo),
        expected_key,
        "grafted-history cache identity must hash the sorted root set"
    );
}

fn git_head(root: &Path) -> String {
    let output = git_command(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse HEAD");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(output.stdout)
        .expect("utf8 git head")
        .trim()
        .to_string()
}

fn clone_checkout(root: &Path) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("create clone dir");
    let clone_root = temp.path().join("clone");
    let mut command = std::process::Command::new("git");
    let status = crate::test_helpers::apply_hermetic_git_env(&mut command)
        .arg("clone")
        .arg("--quiet")
        .arg(root)
        .arg(&clone_root)
        .status()
        .expect("git clone");
    assert!(status.success(), "git clone failed");
    let clone_root = std::fs::canonicalize(clone_root).expect("canonical clone root");
    (temp, clone_root)
}

fn persist_search_index(root: &Path, storage_dir: &Path) {
    let canonical_root = std::fs::canonicalize(root).expect("canonical root");
    let cache_dir = resolve_cache_dir(&canonical_root, Some(storage_dir));
    let mut index = SearchIndex::build(&canonical_root);
    let head = git_head(&canonical_root);
    index.write_to_disk(&cache_dir, Some(&head));
}

fn persist_semantic_index_with_fingerprint(
    root: &Path,
    source_file: &Path,
    storage_dir: &Path,
    fingerprint: SemanticIndexFingerprint,
) {
    let canonical_root = std::fs::canonicalize(root).expect("canonical root");
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let canonical_source = std::fs::canonicalize(source_file).expect("canonical source file");
    let mut index = SemanticIndex::build(&canonical_root, &[canonical_source], &mut embed, 8)
        .expect("build semantic index");
    index.set_fingerprint(fingerprint);
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    index.write_to_disk(storage_dir, &artifact_cache_key(&canonical_root));
}

fn persist_matching_semantic_index(
    root: &Path,
    source_file: &Path,
    storage_dir: &Path,
    base_url: &str,
) {
    persist_semantic_index_with_fingerprint(
        root,
        source_file,
        storage_dir,
        SemanticIndexFingerprint {
            backend: "openai_compatible".to_string(),
            model: "test-embedding".to_string(),
            base_url: base_url.to_string(),
            dimension: 3,
            chunking_version: 2,
            ..Default::default()
        },
    );
}

fn persist_mismatched_semantic_index(root: &Path, source_file: &Path, storage_dir: &Path) {
    persist_semantic_index_with_fingerprint(
        root,
        source_file,
        storage_dir,
        SemanticIndexFingerprint {
            backend: "openai_compatible".to_string(),
            model: "other-model".to_string(),
            base_url: "http://127.0.0.1".to_string(),
            dimension: 3,
            chunking_version: 2,
            ..Default::default()
        },
    );
}

fn openai_context(project_root: &Path, base_url: String) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(project_root.to_path_buf()),
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-embedding".to_string(),
                base_url: Some(base_url),
                api_key_env: None,
                timeout_ms: 5_000,
                query_timeout_ms: 3_000,
                max_batch_size: 64,
                max_files: 20_000,
                ..Default::default()
            },
            ..Config::default()
        }),
    )
}

fn openai_context_with_storage(
    project_root: &Path,
    storage_dir: &Path,
    base_url: String,
) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(project_root.to_path_buf()),
            storage_dir: Some(storage_dir.to_path_buf()),
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-embedding".to_string(),
                base_url: Some(base_url),
                api_key_env: None,
                timeout_ms: 5_000,
                query_timeout_ms: 3_000,
                max_batch_size: 64,
                max_files: 20_000,
                ..Default::default()
            },
            ..Config::default()
        }),
    )
}

fn project_with_needle() -> (tempfile::TempDir, std::path::PathBuf, &'static str) {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    let source = "pub fn needle_symbol() -> bool { true }\npub fn exported() {}\n";
    std::fs::write(&source_file, source).expect("write source file");
    (project, source_file, source)
}

fn install_lexical_index(ctx: &AppContext, source_file: &Path, source: &str) {
    let mut index = SearchIndex::new();
    index.index_file(source_file, source.as_bytes());
    index.ready = true;
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
}

/// With neither index lane ready aft_search refuses with
/// `search_lanes_unavailable`, empty results and each lane's status. The test
/// contexts never build a trigram index, so that lane is enabled but not
/// observed.
fn assert_no_ready_lane_refusal(response: &Value, trigram_status: &str, semantic_status: &str) {
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(
        response["code"], "search_lanes_unavailable",
        "response: {response:?}"
    );
    assert_eq!(response["results"], serde_json::json!([]));
    assert_eq!(response["lanes"]["trigram"]["status"], trigram_status);
    assert_eq!(response["lanes"]["semantic"]["status"], semantic_status);
    assert_eq!(
        response["omitted_lanes"],
        serde_json::json!(["trigram", "semantic"])
    );
}

/// Install a ready trigram index over the whole project, so a test can
/// exercise aft_search routing over a ready lane.
fn install_project_lexical_index(ctx: &AppContext, project_root: &Path) {
    let index = SearchIndex::build(project_root);
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
}

/// Mark the semantic lane ready with an empty resident index. aft_search then
/// has a ready lane while the trigram lane is not, which is when it takes its
/// walk-based literal/regex routes.
fn install_ready_semantic_lane(ctx: &AppContext, project_root: &Path) {
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project_root.to_path_buf(), 3));
}

fn project_with_repeated_needle_files(
    file_count: usize,
) -> (tempfile::TempDir, Vec<(std::path::PathBuf, String)>) {
    let project = tempfile::tempdir().expect("create project dir");
    let src_dir = project.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create source dir");

    let mut entries = Vec::with_capacity(file_count);
    for index in 0..file_count {
        let source_file = src_dir.join(format!("lib_{index}.rs"));
        let source = format!(
            "pub fn needle_symbol_{index}() -> &'static str {{\n    \"needle_symbol\"\n}}\n"
        );
        std::fs::write(&source_file, &source).expect("write source file");
        entries.push((source_file, source));
    }

    (project, entries)
}

fn install_lexical_index_entries(ctx: &AppContext, entries: &[(std::path::PathBuf, String)]) {
    let mut index = SearchIndex::new();
    for (source_file, source) in entries {
        index.index_file(source_file, source.as_bytes());
    }
    index.ready = true;
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
}

fn start_mock_embedding_server() -> (String, thread::JoinHandle<()>) {
    start_mock_embedding_server_with_response(
        "200 OK",
        r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#,
    )
}

fn start_no_request_embedding_server() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind no-request embedding server");
    listener
        .set_nonblocking(true)
        .expect("set no-request server nonblocking");
    let address = listener.local_addr().expect("no-request server address");
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_thread = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    requests_for_thread.fetch_add(1, Ordering::SeqCst);
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request);
                    let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept no-request embedding connection: {error}"),
            }
        }
    });
    (format!("http://{address}"), requests, handle)
}

fn start_mock_embedding_error_server() -> (String, thread::JoinHandle<()>) {
    start_mock_embedding_server_with_response("400 Bad Request", r#"{"error":"embedding boom"}"#)
}

fn start_recovering_slow_embedding_server() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow embedding server");
    let addr = listener.local_addr().expect("slow embedding server addr");
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_thread = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        let mut handlers = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept embedding request");
            let ordinal = requests_for_thread.fetch_add(1, Ordering::SeqCst);
            handlers.push(thread::spawn(move || {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                if ordinal == 0 {
                    thread::sleep(Duration::from_millis(750));
                }
                let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }));
        }
        for handler in handlers {
            handler.join().expect("embedding response handler");
        }
    });

    (format!("http://{addr}"), requests, handle)
}

fn start_mock_embedding_server_with_response(
    status: &str,
    body: &str,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let addr = listener.local_addr().expect("embedding server addr");
    let status = status.to_string();
    let body = body.to_string();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept embedding request");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        let mut content_length = 0usize;

        loop {
            let n = stream.read(&mut chunk).expect("read embedding request");
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

        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    });

    (format!("http://{addr}"), handle)
}

fn path_ends_with(file: &str, suffix: &str) -> bool {
    file.replace('\\', "/").ends_with(suffix)
}

fn assert_lexical_fallback(response: &Value, semantic_status: &str) {
    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["complete"], false);
    assert_eq!(response["semantic_unavailable"], true);
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(response["semantic_status"], semantic_status);
    // The fallback is index-backed and does not call semantic embeddings.
    // Exact evidence may promote lexical candidates, but interpreted_as remains
    // "lexical" to describe the resource that actually executed.
    assert_eq!(response["interpreted_as"], "lexical");
    assert_eq!(response["status"], "ready");
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(
            |result| matches!(result["source"].as_str(), Some("exact" | "lexical"))
                && result["file"]
                    .as_str()
                    .is_some_and(|file| path_ends_with(file, "src/lib.rs"))
        ),
        "expected index-backed fallback result, got {results:?}"
    );
    let warnings = response["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("lexical-only fallback"))),
        "expected lexical fallback warning, got {warnings:?}"
    );
}

fn assert_degraded_grep_fallback(response: &Value, semantic_status: &str) {
    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["complete"], false);
    assert_eq!(response["semantic_unavailable"], true);
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(response["semantic_status"], semantic_status);
    // Honesty: this path ran a literal grep scan (results are GrepLine entries),
    // so interpreted_as must report "literal", not the routed hybrid mode.
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(response["status"], "ready");
    assert_eq!(response["fully_degraded"], true);
    assert_eq!(response["engine_capped"], false);

    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| result["kind"] == "GrepLine"
            && result["file"]
                .as_str()
                .is_some_and(|file| file.replace('\\', "/").ends_with("src/lib.rs"))
            && result["line_text"]
                .as_str()
                .is_some_and(|line| line.contains("needle_symbol"))),
        "expected degraded grep fallback result, got {results:?}"
    );

    let warnings = response["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("lexical-only fallback"))),
        "expected lexical fallback warning, got {warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("degraded full-file-scan"))),
        "expected degraded full-file-scan warning, got {warnings:?}"
    );
}

#[test]
fn natural_language_query_refuses_when_no_lane_is_ready_and_semantic_disabled() {
    // With no ready index lane aft_search refuses with each lane's status;
    // it no longer answers with a degraded filesystem-walk grep.
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::write(
        &source_file,
        "pub fn retry() { /* how retry logic works */ }\n",
    )
    .expect("write source file");
    let ctx = test_context(project.path());
    ctx.update_config(|config| config.indexes.semantic = false);
    let response = response_value(handle_semantic_search(
        &request("how retry logic works"),
        &ctx,
    ));

    assert_no_ready_lane_refusal(&response, "unavailable", "off");
}

#[test]
fn no_ready_lane_refuses_instead_of_a_capped_degraded_grep() {
    // With no ready index lane aft_search refuses with each lane's status;
    // it no longer answers with a degraded filesystem-walk grep.
    let project = tempfile::tempdir().expect("create project dir");
    let ctx = test_context(project.path());
    ctx.update_config(|config| config.indexes.semantic = false);
    let response = response_value(handle_semantic_search(
        &request("how slow backend fallback works"),
        &ctx,
    ));

    assert_no_ready_lane_refusal(&response, "unavailable", "off");
}

#[test]
fn natural_language_query_refuses_while_the_semantic_lane_builds() {
    // With no ready index lane aft_search refuses with each lane's status;
    // it no longer answers with a degraded filesystem-walk grep.
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::write(
        &source_file,
        "pub fn retry() { /* how retry logic works */ }\n",
    )
    .expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
        stage: "embedding".to_string(),
        files: Some(1),
        entries_done: Some(0),
        entries_total: Some(1),
    };
    let response = response_value(handle_semantic_search(
        &request("how retry logic works"),
        &ctx,
    ));

    assert_no_ready_lane_refusal(&response, "unavailable", "building");
}

#[test]
fn blank_queries_are_rejected_before_routing() {
    let project = tempfile::tempdir().expect("create project dir");
    let ctx = test_context(project.path());

    for query in ["", "  "] {
        let response = response_value(handle_semantic_search(&request(query), &ctx));
        assert_eq!(response["success"], false);
        assert_eq!(response["code"], "invalid_request");
        assert_eq!(response["message"], "query must be non-empty");
    }
}

#[test]
fn external_missing_path_returns_path_not_found() {
    let session_project = tempfile::tempdir().expect("session project");
    let missing = session_project.path().join("does-not-exist");
    let ctx = test_context(session_project.path());

    let response = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), &missing),
        &ctx,
    ));

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "path_not_found");
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains(&missing.display().to_string())),
        "expected missing-path message to name the requested path: {response:?}"
    );
}

#[test]
fn external_absent_cache_degrades_to_lexical_fallback_scan() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, external_source, _source) = git_project_with_needle();
    let session_project = tempfile::tempdir().expect("session project");
    let storage = tempfile::tempdir().expect("storage");
    let ctx = test_context_with_storage(session_project.path(), storage.path());

    // This test covers the generic scan, so the query has two words. A query
    // that is one identifier takes the exact identifier sweep instead, which
    // reads every file and is not degraded (see
    // external_identifier_without_index_returns_every_source_use_and_no_dump).
    let response = response_value(handle_semantic_search(
        &request_with_path("fn needle_symbol", Some("literal"), external_project.path()),
        &ctx,
    ));

    // An unindexed foreign root must degrade to a bounded lexical scan with a
    // disclosure — not dead-end with a not_indexed error (which pushes agents
    // to shell out to grep/bash instead of staying on aft_search).
    assert_eq!(response["success"], true, "expected success: {response:?}");
    // The reply says in plain words that no index exists and how much of the
    // project the literal scan read, so it no longer carries the
    // `fully_degraded` flag, whose rendering ("Search status: fully
    // degraded") told the agent nothing it could act on.
    assert_eq!(response["fully_degraded"], false);
    let text = response["text"].as_str().expect("text");
    assert!(
        text.contains("No AFT index exists for")
            && text.contains("It read all 1 text file under")
            && text.contains("Use grep with path for an exhaustive check"),
        "expected the plain no-index coverage paragraph: {text}"
    );
    assert_eq!(response["semantic_status"], "external_unindexed");
    assert_eq!(response["borrowed"], true);
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| {
            result["file"]
                .as_str()
                .is_some_and(|file| external_source.display().to_string().contains(file))
                || result["file"]
                    .as_str()
                    .is_some_and(|file| Path::new(file).file_name() == external_source.file_name())
        }),
        "expected the lexical fallback to actually find needle_symbol in the \
         external repo: {response:?}"
    );
    let warnings = response["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|warning| {
            warning
                .as_str()
                .is_some_and(|text| text.contains("bounded lexical scan"))
        }),
        "expected the no-index disclosure warning: {response:?}"
    );
}

#[test]
fn same_root_path_param_is_byte_identical_to_default_search() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (project, _source_file, _source) = git_project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let without_path = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("literal")),
        &ctx,
    ));
    let subfolder = project.path().join("nested");
    std::fs::create_dir(&subfolder).expect("create subfolder");
    let with_subfolder = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), &subfolder),
        &ctx,
    ));
    assert_eq!(
        with_subfolder, without_path,
        "same-repo subfolder is not a filter"
    );
    let with_path = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), project.path()),
        &ctx,
    ));

    assert_eq!(
        serde_json::to_vec(&with_path).expect("serialize with path"),
        serde_json::to_vec(&without_path).expect("serialize without path"),
        "same-root path must preserve the single-root response contract byte-for-byte"
    );
}

#[test]
fn non_git_same_root_path_param_is_byte_identical_to_default_search() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let without_path = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("literal")),
        &ctx,
    ));
    let with_path = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), project.path()),
        &ctx,
    ));

    assert_eq!(
        serde_json::to_vec(&with_path).expect("serialize with path"),
        serde_json::to_vec(&without_path).expect("serialize without path"),
        "non-Git same-root path must preserve the default response byte-for-byte"
    );
}

#[test]
fn non_git_dot_path_param_is_byte_identical_to_default_search() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let without_path = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("literal")),
        &ctx,
    ));
    let with_path = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), Path::new(".")),
        &ctx,
    ));

    assert_eq!(
        serde_json::to_vec(&with_path).expect("serialize with path"),
        serde_json::to_vec(&without_path).expect("serialize without path"),
        "non-Git dot path must preserve the default response byte-for-byte"
    );
}

#[test]
fn restricted_paths_keep_same_root_local_and_refuse_external_root() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (session_project, _source_file, _source) = project_with_needle();
    let (external_project, _source_file, _source) = git_project_with_needle();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(session_project.path().to_path_buf()),
            restrict_to_project_root: true,
            ..Config::default()
        }),
    );
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let without_path = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("literal")),
        &ctx,
    ));
    let same_root = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), session_project.path()),
        &ctx,
    ));

    assert_eq!(
        serde_json::to_vec(&same_root).expect("serialize same-root response"),
        serde_json::to_vec(&without_path).expect("serialize default response"),
        "path restriction must still allow the configured non-Git root"
    );

    let external_root = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), external_project.path()),
        &ctx,
    ));

    assert_eq!(external_root["success"], false);
    assert_eq!(external_root["code"], "path_outside_root");
}

#[test]
fn external_non_git_path_still_returns_not_a_git_root() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let session_project = tempfile::tempdir().expect("session project");
    let external_project = tempfile::tempdir().expect("external project");
    let ctx = test_context(session_project.path());

    let response = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), external_project.path()),
        &ctx,
    ));

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "not_a_git_root");
    let message = response["message"].as_str().expect("error message");
    assert!(message.contains("another Git project"));
    assert!(message.contains("grep or glob with path"));
}

#[test]
fn external_ignore_rule_mismatch_keeps_drift_in_metadata_only() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (owner_project, _owner_source, _source) = git_project_with_needle();
    let storage = tempfile::tempdir().expect("storage");
    let owner_only_ignore = owner_project.path().join(".foo/.gitignore");
    std::fs::create_dir_all(owner_only_ignore.parent().expect("ignore parent"))
        .expect("create ignore dir");
    std::fs::write(&owner_only_ignore, "# owner-only ignore file\n").expect("write ignore file");
    persist_search_index(owner_project.path(), storage.path());

    let (_clone, sibling_root) = clone_checkout(owner_project.path());
    let session_project = tempfile::tempdir().expect("session project");
    let ctx = test_context_with_storage(session_project.path(), storage.path());

    let response = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), &sibling_root),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "external search should succeed: {response:?}"
    );
    assert_eq!(response["borrowed"], true);
    assert_eq!(response["ignore_rules_differ"], true);
    assert!(
        response["text"].as_str().is_some_and(|text| !text
            .contains("ignore rules differ between checkouts")
            && !text.contains("borrowed index")),
        "borrow metadata must not become agent-facing stale prose: {response:?}"
    );
    let results = response["results"].as_array().expect("results array");
    assert!(
        results
            .iter()
            .any(|result| result["file"].as_str().is_some_and(|file| {
                Path::new(file).is_absolute() && file.replace('\\', "/").ends_with("src/lib.rs")
            })),
        "expected borrowed literal result from sibling checkout: {response:?}"
    );
}

#[test]
fn external_semantic_search_hides_drift_prose_and_refreshes_file_summary_snippet() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, external_source, _source) = git_project_with_needle();
    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external_project.path(), storage.path());
    let (base_url, handle) = start_mock_embedding_server();
    persist_matching_semantic_index(
        external_project.path(),
        &external_source,
        storage.path(),
        &base_url,
    );
    std::fs::write(
        &external_source,
        "// updated after index build\npub fn needle_symbol() -> bool { false }\npub fn exported() {}\n",
    )
    .expect("mutate external source");
    let session_project = tempfile::tempdir().expect("session project");
    let ctx = openai_context_with_storage(session_project.path(), storage.path(), base_url);
    let response = response_value(handle_semantic_search(
        &request_with_path(
            "where is needle_symbol implemented today",
            Some("semantic"),
            external_project.path(),
        ),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "external semantic search should succeed: {response:?}"
    );
    assert_eq!(response["borrowed"], true);
    assert_eq!(response["semantic_status"], "ready");
    assert_eq!(
        response["drift_count"], 0,
        "borrowed requests no longer census the external corpus"
    );
    let text = response["text"].as_str().expect("text");
    assert!(
        !text.contains("borrowed index"),
        "unexpected borrow prose: {response:?}"
    );
    assert!(
        !text.contains("drift"),
        "unexpected drift prose: {response:?}"
    );
    assert!(
        text.contains("pub fn needle_symbol() -> bool { false }"),
        "FileSummary snippet should be regenerated from current disk content: {response:?}"
    );
    assert!(
        text.contains("The saved semantic index of")
            && text.contains("predates 1 file that changed or was added since"),
        "the semantic lane answered from vectors older than the edit, and says so: {text}"
    );
    handle.join().expect("embedding server thread");
}

#[test]
fn external_semantic_fingerprint_mismatch_returns_lexical_only_note() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, external_source, _source) = git_project_with_needle();
    let session_project = tempfile::tempdir().expect("session project");
    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external_project.path(), storage.path());
    persist_mismatched_semantic_index(external_project.path(), &external_source, storage.path());
    let ctx = test_context_with_storage(session_project.path(), storage.path());

    // This test covers the ranked lanes, so the query has two words. A query
    // that is one identifier takes the exact identifier sweep instead, which
    // has no semantic lane to miss and so is not partial.
    let response = response_value(handle_semantic_search(
        &request_with_path("fn needle_symbol", None, external_project.path()),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "external search should succeed with lexical fallback: {response:?}"
    );
    assert_eq!(response["complete"], false);
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(response["semantic_status"], "unavailable");
    assert_eq!(response["interpreted_as"], "lexical");
    assert_eq!(
        response["external_root"],
        std::fs::canonicalize(external_project.path())
            .expect("canonical external root")
            .display()
            .to_string()
    );
    assert!(
        response["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("different embedding backend or model"))),
        "expected fingerprint mismatch warning: {response:?}"
    );
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| result["source"] == "exact"
            && result["file"].as_str().is_some_and(|file| {
                Path::new(file).is_absolute() && file.replace('\\', "/").ends_with("src/lib.rs")
            })),
        "expected absolute exact-lane result from external project: {response:?}"
    );
}

#[test]
fn external_path_obeys_force_restrict_guard() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, _external_source, _source) = git_project_with_needle();
    let session_project = tempfile::tempdir().expect("session project");
    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external_project.path(), storage.path());
    let ctx = test_context_with_storage(session_project.path(), storage.path());
    let request = request_with_path("needle_symbol", Some("literal"), external_project.path());

    let unrestricted = response_value(handle_semantic_search(&request, &ctx));
    assert_eq!(
        unrestricted["success"], true,
        "external search should proceed without force restriction: {unrestricted:?}"
    );
    assert!(
        unrestricted["results"]
            .as_array()
            .expect("results array")
            .iter()
            .any(|result| result["file"].as_str().is_some_and(|file| {
                Path::new(file).is_absolute() && file.replace('\\', "/").ends_with("src/lib.rs")
            })),
        "expected absolute external result before force restriction: {unrestricted:?}"
    );

    let restricted = ctx.with_force_restrict(&request.id, || {
        response_value(handle_semantic_search(&request, &ctx))
    });
    assert_eq!(restricted["success"], false);
    assert_eq!(restricted["code"], "path_outside_root");
    assert!(
        restricted["message"]
            .as_str()
            .is_some_and(|message| message.contains("path restriction is enabled")),
        "expected force-restrict error message: {restricted:?}"
    );
}

#[test]
fn hybrid_disabled_semantic_uses_lexical_only_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_lexical_fallback(&response, "disabled");
}

#[test]
fn natural_language_exact_phrase_marks_rank_one_lexical_fallback() {
    let project = tempfile::tempdir().expect("create project dir");
    let target = project.path().join("src/tool/browser.rs");
    let partial = project.path().join("src/tool/other.rs");
    std::fs::create_dir_all(target.parent().expect("source parent")).expect("create source dir");
    let sentence = "The feature is not wired into the built-in browser tool yet.";
    std::fs::write(&target, format!("// {sentence}\n")).expect("write target");
    std::fs::write(&partial, "// browser tool wiring\n").expect("write partial match");
    let entries = vec![
        (target.clone(), format!("// {sentence}\n")),
        (partial, "// browser tool wiring\n".to_string()),
    ];
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let exact_query = format!("\"{sentence}\" where is this");
    let response = response_value(handle_semantic_search(
        &request_with_top_k(&exact_query, None, 5),
        &ctx,
    ));
    let first = &response["results"][0];
    assert!(path_ends_with(
        first["file"].as_str().expect("result file"),
        "src/tool/browser.rs"
    ));
    assert_eq!(first["exact"], true);
    assert!(response["text"]
        .as_str()
        .expect("rendered response")
        .contains("src/tool/browser.rs:1 [exact]"));
}

/// A census-style JSON document that quotes a phrase twice, next to the one
/// source file that emits it.
fn project_with_phrase_in_source_and_data() -> (tempfile::TempDir, Vec<(std::path::PathBuf, String)>)
{
    let project = tempfile::tempdir().expect("create project dir");
    let source = project.path().join("src/configure.rs");
    let data = project.path().join("benchmarks/census_episodes.json");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create source dir");
    std::fs::create_dir_all(data.parent().expect("data parent")).expect("create data dir");
    let source_text =
        "pub fn verify_tool(tool: &str) {\n    warn!(\"configured tool {} was not found on PATH\", tool);\n}\n"
            .to_string();
    let body = "x".repeat(300);
    let data_text = format!(
        "{{\n  \"query\": \"was not found on PATH\",\n  \"file_content\": \"{body} was not found on PATH\"\n}}\n"
    );
    std::fs::write(&source, &source_text).expect("write source");
    std::fs::write(&data, &data_text).expect("write data");
    (project, vec![(data, data_text), (source, source_text)])
}

fn ranked_files(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|result| {
            result["file"]
                .as_str()
                .expect("result file")
                .replace('\\', "/")
        })
        .collect()
}

/// Only `scripts/check.py` contains the literal `residue-source-hash`. The other
/// two files hold its parts: `src/server.rs` declares a field named `source`,
/// and `docs/notes.md` mentions all three words within three lines. Before
/// hyphenated queries were verified as literals, both of those ranked above
/// the file holding the string and were labelled `[exact]`.
fn project_with_hyphenated_literal() -> (tempfile::TempDir, Vec<(std::path::PathBuf, String)>) {
    let project = tempfile::tempdir().expect("create project dir");
    let files = [
        (
            "src/server.rs",
            "pub struct Registry {\n    source: String,\n}\n",
        ),
        (
            "docs/notes.md",
            "The residue left behind\nby the source\nchanges its hash.\n",
        ),
        (
            "scripts/check.py",
            "CHECKS = [\"residue-source-hash\", \"slice-fences\"]\n",
        ),
    ];
    let mut entries = Vec::new();
    for (relative, text) in files {
        let path = project.path().join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(&path, text).expect("write file");
        entries.push((path, text.to_string()));
    }
    (project, entries)
}

/// Every result marked exact, in the JSON and in the rendered `[exact]`
/// header, must be a file that contains `literal` verbatim.
fn assert_exact_only_on_files_containing(response: &Value, literal: &str) {
    let mut exact_files = Vec::new();
    for result in response["results"].as_array().expect("results array") {
        let file = result["file"].as_str().expect("result file");
        if result["exact"] == true {
            let text = std::fs::read_to_string(file).expect("read exact result");
            assert!(
                text.contains(literal),
                "{file} is marked exact but does not contain {literal:?}: {response}"
            );
            exact_files.push(file.replace('\\', "/"));
        }
    }
    let rendered = response["text"].as_str().expect("rendered response");
    for line in rendered.lines().filter(|line| line.contains("[exact]")) {
        // A header reads `<path>[:line] [exact]`; local replies show the path
        // relative to the project, external ones show it absolute.
        let shown = line
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .replace('\\', "/");
        let shown = shown
            .rsplit_once(':')
            .filter(|(_, line_number)| line_number.chars().all(|c| c.is_ascii_digit()))
            .map_or(shown.as_str(), |(path, _)| path)
            .to_string();
        assert!(
            exact_files.iter().any(|file| file.ends_with(&shown)),
            "[exact] header on a file that does not contain {literal:?}: {line}\n{rendered}"
        );
    }
}

#[test]
fn hyphenated_literal_ranks_the_containing_file_first_and_only_it_is_exact() {
    let (project, entries) = project_with_hyphenated_literal();
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with_top_k("residue-source-hash", None, 5),
        &ctx,
    ));

    let files = ranked_files(&response);
    assert!(files[0].ends_with("scripts/check.py"), "ranked: {files:?}");
    assert_eq!(response["results"][0]["exact"], true, "{response}");
    assert!(response["text"]
        .as_str()
        .expect("rendered response")
        .contains("[exact]"));
    assert_exact_only_on_files_containing(&response, "residue-source-hash");
}

#[test]
fn external_hyphenated_literal_ranks_the_containing_file_first_and_only_it_is_exact() {
    // A path naming another Git project searches that project's persisted
    // (borrowed) index instead of the session's own, so the literal routing
    // must hold on that path too.
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, _entries) = project_with_hyphenated_literal();
    init_git(external_project.path());
    commit_all(external_project.path());
    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external_project.path(), storage.path());
    let session_project = tempfile::tempdir().expect("session project");
    let ctx = test_context_with_storage(session_project.path(), storage.path());

    let response = response_value(handle_semantic_search(
        &request_with_path("residue-source-hash", None, external_project.path()),
        &ctx,
    ));

    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["borrowed"], true, "{response}");
    let files = ranked_files(&response);
    assert!(files[0].ends_with("scripts/check.py"), "ranked: {files:?}");
    assert_eq!(response["results"][0]["exact"], true, "{response}");
    assert_exact_only_on_files_containing(&response, "residue-source-hash");
}

/// Write `files` (project-relative path, text) under a fresh project and
/// return it with the entries for `install_lexical_index_entries`.
fn project_with_files(
    files: &[(&str, &str)],
) -> (tempfile::TempDir, Vec<(std::path::PathBuf, String)>) {
    let project = tempfile::tempdir().expect("create project dir");
    // The canonical root (macOS temp dirs sit behind a /var symlink) so the
    // indexed paths match the root the reply strips from displayed paths.
    let root = std::fs::canonicalize(project.path()).expect("canonical project dir");
    let mut entries = Vec::new();
    for (relative, text) in files {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(&path, text).expect("write file");
        entries.push((path, text.to_string()));
    }
    (project, entries)
}

/// Run `query` over `files` with a ready trigram index and no semantic lane.
fn lexical_search(files: &[(&str, &str)], query: &str) -> (tempfile::TempDir, Value) {
    let (project, entries) = project_with_files(files);
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
    let response = response_value(handle_semantic_search(
        &request_with_top_k(query, None, 10),
        &ctx,
    ));
    assert_eq!(response["success"], true, "{response}");
    (project, response)
}

#[test]
fn definition_hit_is_rendered_at_its_declaration_line() {
    // Line 1 holds a longer name containing the query; the declaration of
    // `running_tasks` itself is on line 5. The definition hit must show line 5.
    let (_project, response) = lexical_search(
        &[(
            "src/registry.rs",
            "pub fn kill_running_tasks_for_root(root: &str) {\n    drop(root);\n}\n\npub fn running_tasks(&self) -> usize {\n    0\n}\n",
        )],
        "running_tasks",
    );

    let first = &response["results"][0];
    assert!(
        path_ends_with(first["file"].as_str().unwrap(), "src/registry.rs"),
        "{response}"
    );
    assert_eq!(first["exact"], true, "{response}");
    assert_eq!(first["start_line"], 5, "{response}");
    assert!(
        first["snippet"]
            .as_str()
            .unwrap()
            .contains("pub fn running_tasks(&self)"),
        "{response}"
    );
    // The rendered header uses the platform's path separator.
    let header = format!("{}:5", Path::new("src").join("registry.rs").display());
    assert!(
        response["text"].as_str().unwrap().contains(&header),
        "{response}"
    );
}

#[test]
fn source_declaration_outranks_a_document_that_quotes_it() {
    // The note quotes the declaration and sorts first by path (`.` < `c`);
    // only the Rust file declares the function.
    let (_project, response) = lexical_search(
        &[
            (
                ".gsd/milestones/S05-RESEARCH.md",
                "Planned helper:\n\nfn line_col_to_byte(source: &str, line: u32, col: u32) -> usize {\n",
            ),
            (
                "crates/aft/src/edit.rs",
                "pub fn line_col_to_byte(source: &str, line: u32, col: u32) -> usize {\n    0\n}\n",
            ),
        ],
        "line_col_to_byte",
    );

    let files = ranked_files(&response);
    assert!(
        files[0].ends_with("crates/aft/src/edit.rs"),
        "ranked: {files:?}"
    );
    assert!(files[1].ends_with("S05-RESEARCH.md"), "ranked: {files:?}");
}

#[test]
fn receiver_member_query_ranks_the_member_declaration_first() {
    // `ask.list_pending_for_user` reads like its call site; the declaration
    // says `fn list_pending_for_user`. The file whose path names the receiver
    // comes first even though the other declaration's path sorts before it,
    // then that other declaration, then the call site.
    let (_project, response) = lexical_search(
        &[
            (
                "src/handlers.rs",
                "fn handle() {\n    ask.list_pending_for_user(user);\n}\n",
            ),
            (
                "src/ask/store.rs",
                "impl AskStore {\n    pub fn list_pending_for_user(&self, user: &str) {}\n}\n",
            ),
            (
                "src/admin/registry.rs",
                "pub fn list_pending_for_user(user: &str) {}\n",
            ),
        ],
        "ask.list_pending_for_user",
    );

    let files = ranked_files(&response);
    assert!(files[0].ends_with("src/ask/store.rs"), "ranked: {files:?}");
    assert!(
        files[1].ends_with("src/admin/registry.rs"),
        "ranked: {files:?}"
    );
    assert!(files[2].ends_with("src/handlers.rs"), "ranked: {files:?}");
    assert_eq!(response["results"][0]["start_line"], 2, "{response}");
}

#[test]
fn missing_identifier_answers_not_found_with_nearest_names() {
    let (_project, response) = lexical_search(
        &[
            (
                "src/state.rs",
                "pub fn mark_file_refreshed(id: u32) {}\npub fn mark_file_stale(id: u32) {}\n",
            ),
            (
                "src/caller.rs",
                "fn run() {\n    mark_file_refreshed(1);\n}\n",
            ),
        ],
        "mark_file_refreshing",
    );

    let text = response["text"].as_str().unwrap();
    // Locations use the platform's path separator, like result headers.
    let state = Path::new("src").join("state.rs").display().to_string();
    assert!(
        text.starts_with(&format!(
            "`mark_file_refreshing` not found in this project. Nearest names: \
             `mark_file_refreshed` ({state}:1), `mark_file_stale` ({state}:2)"
        )),
        "{text}"
    );
    let results = response["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{response}");
    assert_eq!(results[0]["name"], "mark_file_refreshed", "{response}");
    assert_eq!(results[0]["source"], "nearest_name", "{response}");
    assert_eq!(results[0]["start_line"], 1, "{response}");
    assert_eq!(response["more_available"], false, "{response}");
}

#[test]
fn identifier_that_occurs_anywhere_keeps_the_ordinary_ranking() {
    // `refresh_all` occurs only inside a longer name, and `rebuild_cache`
    // only in a test file the request excludes. Neither is absent from the
    // project, so neither gets a not-found answer.
    let files = [
        ("src/state.rs", "pub fn refresh_all_files() {}\n"),
        ("tests/cache_test.rs", "fn rebuild_cache() {}\n"),
        ("src/cache.rs", "pub fn rebuild_index() {}\n"),
    ];
    for query in ["refresh_all", "rebuild_cache"] {
        let (_project, response) = lexical_search(&files, query);
        let text = response["text"].as_str().unwrap();
        assert!(
            !text.contains("not found in this project"),
            "{query}: {text}"
        );
        assert!(
            response["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|result| result["source"] != "nearest_name"),
            "{query}: {response}"
        );
    }
}

#[test]
fn quoted_phrase_ranks_source_above_data_file_that_repeats_it() {
    let (project, entries) = project_with_phrase_in_source_and_data();
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with_top_k("\"was not found on PATH\"", None, 5),
        &ctx,
    ));
    let files = ranked_files(&response);
    assert!(files[0].ends_with("src/configure.rs"), "ranked: {files:?}");
    // Demoted, not hidden: the data file is still in the list.
    assert!(
        files
            .iter()
            .any(|file| file.ends_with("benchmarks/census_episodes.json")),
        "ranked: {files:?}"
    );
}

#[test]
fn query_naming_the_data_file_keeps_it_first() {
    let (project, entries) = project_with_phrase_in_source_and_data();
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with_top_k("\"was not found on PATH\" json", None, 5),
        &ctx,
    ));
    let files = ranked_files(&response);
    assert!(
        files[0].ends_with("benchmarks/census_episodes.json"),
        "ranked: {files:?}"
    );
}

#[test]
fn hybrid_failed_semantic_uses_lexical_only_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        SemanticIndexStatus::Failed("ONNX Runtime unavailable".to_string());

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_lexical_fallback(&response, "unavailable");
}

#[test]
fn auto_mode_refuses_when_trigram_unavailable_and_semantic_disabled() {
    // With no ready index lane aft_search refuses with each lane's status;
    // it no longer answers with a degraded filesystem-walk grep.
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    ctx.update_config(|config| config.indexes.semantic = false);
    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_no_ready_lane_refusal(&response, "unavailable", "off");
}

#[test]
fn embed_query_failure_falls_back_to_grep_when_hint_not_explicit_semantic() {
    let (project, _source_file, _source) = project_with_needle();
    let (base_url, handle) = start_mock_embedding_error_server();
    let ctx = openai_context(project.path(), base_url);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_degraded_grep_fallback(&response, "unavailable");
    handle.join().expect("embedding server thread");
}

#[test]
fn slow_query_embedding_degrades_within_budget_and_next_query_retries_fresh() {
    let (project, source_file, source) = project_with_needle();
    let (base_url, requests, handle) = start_recovering_slow_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    ctx.update_config(|config| config.semantic.query_timeout_ms = 500);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let started = Instant::now();
    let semantic_request = request_with(
        "where is the needle symbol implementation",
        Some("semantic"),
    );
    let degraded = response_value(handle_semantic_search(&semantic_request, &ctx));
    let elapsed = started.elapsed();

    assert_lexical_fallback(&degraded, "unavailable");
    assert!(
        degraded["text"].as_str().is_some_and(|text| text
            .contains("Semantic search unavailable; returning lexical-only fallback results.")),
        "existing degraded disclosure must remain byte-identical: {degraded:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1_000),
        "500ms query budget took {elapsed:?}"
    );

    let recovered = response_value(handle_semantic_search(&semantic_request, &ctx));
    assert_eq!(recovered["success"], true, "response: {recovered:?}");
    assert_eq!(recovered["complete"], true, "response: {recovered:?}");
    handle.join().expect("recovering embedding server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "each search should issue one fresh embedding request"
    );
}

#[test]
fn legacy_semantic_hint_without_any_index_refuses() {
    // With no ready index lane aft_search refuses with each lane's status;
    // it no longer answers with a degraded filesystem-walk grep.
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    ctx.update_config(|config| config.indexes.semantic = false);
    let response = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("semantic")),
        &ctx,
    ));

    assert_no_ready_lane_refusal(&response, "unavailable", "off");
}

#[test]
fn legacy_semantic_hint_is_ignored_with_lexical_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("semantic")),
        &ctx,
    ));

    assert_eq!(response["success"], true);
    assert_eq!(response["lexical_only_fallback"], true);
}

#[test]
fn regex_grep_success_reports_ready_status_not_semantic_backend_status() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());
    let response = response_value(handle_semantic_search(
        &request_with("^pub fn exported", Some("regex")),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["complete"], true);
    assert_eq!(response["results"][0]["kind"], "GrepLine");
}

#[test]
fn grep_results_report_regex_or_literal_source() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());

    // Only the regex route yields grep lines once a trigram lane is ready; a
    // literal query is then served by the lexical lane (see
    // literal_query_strips_surrounding_paired_quotes).
    for (query, hint, expected_source) in [("^pub fn exported", "regex", "regex")] {
        let response = response_value(handle_semantic_search(
            &request_with(query, Some(hint)),
            &ctx,
        ));

        assert_eq!(
            response["success"], true,
            "{hint} grep query should succeed: {response:?}"
        );
        assert_eq!(response["interpreted_as"], expected_source);
        let results = response["results"].as_array().expect("results array");
        assert!(!results.is_empty(), "expected {hint} grep results");
        for result in results {
            assert_eq!(result["kind"], "GrepLine");
            assert_eq!(result["source"], expected_source);
            assert_ne!(result["source"], "hybrid");
            assert!(
                result.get("line_text").is_some(),
                "line_text field should remain present: {result:?}"
            );
            assert!(
                result.get("match_text").is_some(),
                "match_text field should remain present: {result:?}"
            );
        }
    }
}

#[test]
fn standalone_grep_keeps_generated_artifacts_in_mtime_order() {
    let project = tempfile::tempdir().expect("create project dir");
    let source = project.path().join("Source/Session.swift");
    let html = project.path().join("docs/index.html");
    let json = project.path().join("docs/search.json");
    let css = project.path().join("docs/style.css");

    for path in [&source, &html, &json, &css] {
        std::fs::create_dir_all(path.parent().expect("fixture parent"))
            .expect("create fixture dir");
        std::fs::write(path, "StandaloneGrepNeedle\n").expect("write fixture file");
    }
    for (path, seconds) in [
        (&source, 1_700_000_000),
        (&css, 1_700_000_100),
        (&json, 1_700_000_200),
        (&html, 1_700_000_300),
    ] {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0))
            .expect("set fixture mtime");
    }

    let ctx = test_context(project.path());
    let response = response_value(handle_grep(&grep_request("StandaloneGrepNeedle", 10), &ctx));

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    let matches = response["matches"].as_array().expect("matches array");
    let files = matches
        .iter()
        .map(|result| {
            result["file"]
                .as_str()
                .expect("grep match file")
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();

    assert!(
        files.iter().any(|file| file.ends_with("docs/index.html"))
            && files.iter().any(|file| file.ends_with("docs/search.json"))
            && files.iter().any(|file| file.ends_with("docs/style.css")),
        "standalone grep should keep generated artifacts findable: {files:?}"
    );
    assert!(
        files
            .first()
            .is_some_and(|file| file.ends_with("docs/index.html")),
        "standalone grep should keep normal mtime ordering instead of search demotion: {files:?}"
    );
}

#[test]
fn standalone_grep_uses_display_path_tiebreak_for_equal_mtimes() {
    let project = tempfile::tempdir().expect("create project dir");
    let src = project.path().join("src");
    std::fs::create_dir_all(&src).expect("create source dir");
    let zeta = src.join("zeta.txt");
    let alpha = src.join("alpha.txt");
    std::fs::write(&zeta, "EqualMtimeNeedle\n").expect("write zeta fixture");
    std::fs::write(&alpha, "EqualMtimeNeedle\n").expect("write alpha fixture");

    let fixed_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    for path in [&zeta, &alpha] {
        filetime::set_file_mtime(path, fixed_mtime).expect("set identical fixture mtime");
    }

    let ctx = test_context(project.path());
    let first = response_value(handle_grep(&grep_request("EqualMtimeNeedle", 10), &ctx));
    let second = response_value(handle_grep(&grep_request("EqualMtimeNeedle", 10), &ctx));

    assert_eq!(
        first["success"], true,
        "first grep should succeed: {first:?}"
    );
    assert_eq!(
        second["success"], true,
        "second grep should succeed: {second:?}"
    );
    let first_matches = first["matches"].as_array().expect("first matches array");
    assert_eq!(
        first_matches.len(),
        2,
        "expected both tie fixtures: {first:?}"
    );
    assert_eq!(
        serde_json::to_vec(&first["matches"]).expect("serialize first matches"),
        serde_json::to_vec(&second["matches"]).expect("serialize second matches"),
        "equal-mtime grep order should be byte-identical across runs"
    );

    let files = first_matches
        .iter()
        .map(|result| {
            result["file"]
                .as_str()
                .expect("grep match file")
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    assert!(
        files[0].ends_with("src/alpha.txt") && files[1].ends_with("src/zeta.txt"),
        "equal-mtime files should sort by normalized display path: {files:?}"
    );
}

#[test]
fn literal_grep_filters_test_support_files_unless_requested() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    let fixture_file = project.path().join("fixtures/schema.sql");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::create_dir_all(fixture_file.parent().expect("fixture parent"))
        .expect("create fixture dir");
    std::fs::write(&fixture_file, "CREATE TABLE needle_table(id int);\n")
        .expect("write fixture file");
    std::fs::write(
        &source_file,
        "pub fn build_schema() { /* CREATE TABLE needle_table */ }\n",
    )
    .expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());
    let default_response = response_value(handle_semantic_search(
        &request_with("CREATE TABLE needle_table", Some("literal")),
        &ctx,
    ));
    assert_eq!(
        default_response["success"], true,
        "default grep should succeed"
    );
    let default_results = default_response["results"]
        .as_array()
        .expect("results array");
    assert!(
        default_results.iter().all(|result| result["file"]
            .as_str()
            .is_some_and(|file| !file.replace('\\', "/").contains("/fixtures/"))),
        "default grep should hide fixtures: {default_response:?}"
    );
    assert!(default_results.iter().any(|result| result["file"]
        .as_str()
        .is_some_and(|file| file.replace('\\', "/").ends_with("src/lib.rs"))));

    let include_response = response_value(handle_semantic_search(
        &request_with_include_tests("CREATE TABLE needle_table", Some("literal"), 5, true),
        &ctx,
    ));
    assert_eq!(
        include_response["success"], true,
        "include_tests grep should succeed"
    );
    let include_results = include_response["results"]
        .as_array()
        .expect("results array");
    assert!(
        include_results.iter().any(|result| result["file"]
            .as_str()
            .is_some_and(|file| file.replace('\\', "/").ends_with("fixtures/schema.sql"))),
        "include_tests:true should surface fixtures: {include_response:?}"
    );
}

#[test]
fn no_ready_lane_refuses_instead_of_a_degraded_grep_with_test_files() {
    // With no ready index lane aft_search refuses with each lane's status;
    // it no longer answers with a degraded filesystem-walk grep.
    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join("fixtures")).expect("create fixture dir");
    std::fs::write(
        project.path().join("fixtures/notes.txt"),
        "how retry schema fallback works\n",
    )
    .expect("write fixture file");
    let ctx = test_context(project.path());
    ctx.update_config(|config| config.indexes.semantic = false);
    let response = response_value(handle_semantic_search(
        &request_with_include_tests("how retry schema fallback works", None, 5, true),
        &ctx,
    ));

    assert_no_ready_lane_refusal(&response, "unavailable", "off");
}

#[test]
fn hybrid_semantic_contribution_reports_separate_boost_metadata() {
    let (project, source_file, source) = project_with_needle();
    let source = format!("{source}// found\n");
    std::fs::write(&source_file, &source).expect("write hybrid source");
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&source_file),
        &mut embed,
        16,
    )
    .expect("build semantic index");
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, &source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let query = "where is needle_symbol found";
    let mut hybrid_request = request(query);
    hybrid_request.id = "hybrid-semantic-contribution".to_string();
    let response = response_value(handle_semantic_search(&hybrid_request, &ctx));

    assert_eq!(
        response["success"], true,
        "hybrid semantic query should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["interpreted_as"], "hybrid");
    let results = response["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "expected hybrid semantic results");

    for result in results {
        let source = result["source"].as_str().expect("result source string");
        assert!(
            matches!(source, "exact" | "semantic" | "lexical"),
            "hybrid response source must identify its winning engine lane, got {source:?}: {result:?}"
        );
        assert_ne!(source, "hybrid");
    }

    let contributed = results
        .iter()
        .find(|result| result["semantic_score"].is_number() && result["lexical_score"].is_number())
        .expect("one result should carry separate semantic and lexical scores");
    assert_eq!(contributed["hybrid_boosted"], true);
    handle.join().expect("embedding server thread");

    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
        stage: "test_unavailable".to_string(),
        files: None,
        entries_done: None,
        entries_total: None,
    };
    let mut lexical_request = request(query);
    lexical_request.id = "hybrid-semantic-unavailable-control".to_string();
    let lexical = response_value(handle_semantic_search(&lexical_request, &ctx));
    let lexical_result = &lexical["results"][0];
    assert!(lexical_result["semantic_score"].is_null());
    assert_eq!(lexical_result["hybrid_boosted"], false);
}

#[test]
fn lexical_only_fallback_pages_beyond_the_old_candidate_cap() {
    let (project, entries) = project_with_repeated_needle_files(6);
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with_top_k("needle symbol needle symbol", None, 5),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "unavailable lexical fallback should succeed: {response:?}"
    );
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(response["engine_capped"], false);
    assert_eq!(response["result_count"], 5);
    assert_eq!(response["more_available"], true);

    let (project, entries) = project_with_repeated_needle_files(210);
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
        stage: "embedding".to_string(),
        files: Some(210),
        entries_done: Some(0),
        entries_total: Some(210),
    };

    let response = response_value(handle_semantic_search(
        &request_with_top_k("needle symbol needle symbol", None, 100),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "building lexical fallback should succeed: {response:?}"
    );
    assert_eq!(response["status"], "building");
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(
        response["engine_capped"], false,
        "the block engine can page beyond the old fixed lexical candidate cap"
    );
    assert_eq!(response["more_available"], true);
}

#[test]
fn identifier_ready_reports_more_available_when_lexical_fallback_is_capped_with_semantic() {
    let (project, entries) = project_with_repeated_needle_files(210);
    let (base_url, embedding_requests, handle) = start_no_request_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let mut identifier_request = request_with_top_k("needle_symbol", None, 100);
    identifier_request.id = "identifier-ready-fallback-cap-with-semantic".to_string();
    let raw_response = handle_semantic_search(&identifier_request, &ctx);
    let rendered = aft::subc_format::format_response("search", &raw_response, false);
    let response = response_value(raw_response);
    handle.join().expect("negative embedding server thread");

    assert_eq!(
        response["success"], true,
        "ready identifier query should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["complete"], true);
    assert_eq!(response["interpreted_as"], "hybrid");
    assert_eq!(response["engine_capped"], true);
    assert_eq!(response["more_available"], true);
    // `query` keeps the semantic lane for an identifier too: one embedding.
    assert_eq!(embedding_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        response["structuredContent"]["search"]["embedding_calls"],
        1
    );
    assert_eq!(
        response["structuredContent"]["search"]["embedding_cache_hits"],
        0
    );
    assert_eq!(
        response["structuredContent"]["search"]["live_embed_calls"],
        1
    );
    assert!(rendered
        .ends_with("shown 100 of ≥210 results (cap) · narrow: offset, topK, path, includeTests"));
}

#[test]
fn semantic_ready_reports_more_available_when_semantic_lane_overflows() {
    let (project, entries) = project_with_repeated_needle_files(101);
    let files = entries
        .iter()
        .map(|(source_file, _)| source_file.clone())
        .collect::<Vec<_>>();
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index =
        SemanticIndex::build(project.path(), &files, &mut embed, 16).expect("build semantic index");
    assert!(
        semantic_index.entry_count() > 100,
        "test setup must exceed the response limit"
    );
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let response = response_value(handle_semantic_search(
        &request_with_top_k(
            "where is the needle symbol implementation",
            Some("semantic"),
            100,
        ),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "ready semantic query should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["complete"], true);
    assert_eq!(response["interpreted_as"], "semantic");
    assert_eq!(response["engine_capped"], false);
    assert_eq!(response["result_count"], 100);
    assert_eq!(response["more_available"], true);
    handle.join().expect("embedding server thread");
}

#[test]
fn excluded_test_results_do_not_starve_default_semantic_search() {
    let project = tempfile::tempdir().expect("create project dir");
    let tests_dir = project.path().join("tests");
    std::fs::create_dir_all(&tests_dir).expect("create tests dir");

    let mut files = Vec::new();
    for index in 0..102 {
        let test_file = tests_dir.join(format!("crowding_{index}_test.rs"));
        std::fs::write(
            &test_file,
            format!("pub fn crowding_symbol_{index}() -> bool {{ true }}\n"),
        )
        .expect("write test source");
        files.push(test_file);
    }

    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::write(
        &source_file,
        "pub fn production_semantic_target() -> bool { true }\n",
    )
    .expect("write production source");
    files.push(source_file.clone());

    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index =
        SemanticIndex::build(project.path(), &files, &mut embed, 32).expect("build semantic index");
    let unfiltered = semantic_index.search(&[0.1, 0.2, 0.3], 101);
    assert_eq!(
        unfiltered.len(),
        101,
        "test setup must fill the fetch window"
    );
    assert!(
        unfiltered
            .iter()
            .all(|result| result.file.starts_with(&tests_dir)),
        "test setup must crowd the unfiltered semantic fetch with hidden tests"
    );

    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let response = response_value(handle_semantic_search(
        &request_with_top_k(
            "where is the production semantic target implemented",
            None,
            5,
        ),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "semantic query failed: {response:?}"
    );
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| result["file"]
            .as_str()
            .is_some_and(|file| path_ends_with(file, "src/lib.rs"))),
        "hidden tests must not crowd the production result out of the semantic fetch: {response:?}"
    );
    assert!(
        results.iter().all(|result| !result["file"]
            .as_str()
            .is_some_and(|file| file.replace('\\', "/").contains("/tests/"))),
        "default search must continue to hide test files: {response:?}"
    );
    handle.join().expect("embedding server thread");
}

#[test]
fn identifier_ready_reports_no_more_available_when_under_top_k_with_semantic() {
    let (project, source_file, source) = project_with_needle();
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&source_file),
        &mut embed,
        16,
    )
    .expect("build semantic index");
    let (base_url, embedding_requests, handle) = start_no_request_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let mut identifier_request = request_with_top_k("needle_symbol", None, 5);
    identifier_request.id = "identifier-ready-under-top-k-with-semantic".to_string();
    let response = response_value(handle_semantic_search(&identifier_request, &ctx));
    handle.join().expect("negative embedding server thread");

    assert_eq!(
        response["success"], true,
        "ready identifier query should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["complete"], true);
    assert_eq!(response["interpreted_as"], "hybrid");
    assert_eq!(response["engine_capped"], false);
    assert!(
        response["result_count"].as_u64().expect("result_count") < 5,
        "test setup should stay under top_k: {response:?}"
    );
    assert_eq!(response["more_available"], false);
    // `query` keeps the semantic lane for an identifier too: one embedding.
    assert_eq!(embedding_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        response["structuredContent"]["search"]["embedding_calls"],
        1
    );
    assert_eq!(
        response["structuredContent"]["search"]["embedding_cache_hits"],
        0
    );
    assert_eq!(
        response["structuredContent"]["search"]["live_embed_calls"],
        1
    );
}

#[test]
fn auto_bare_quantifier_queries_route_to_regex_grep() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::write(&source_file, "foo foobar color colour fooooooobar\n")
        .expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());
    for query in ["foo*", "foo+", "colou?r", "foo*bar"] {
        let response = response_value(handle_semantic_search(&request(query), &ctx));

        assert_eq!(
            response["success"], true,
            "auto regex query should succeed for {query:?}: {response:?}"
        );
        assert_eq!(response["interpreted_as"], "regex", "query: {query:?}");
        assert_eq!(response["query_kind"], "Regex", "query: {query:?}");
        assert_eq!(response["semantic_status"], "disabled", "query: {query:?}");
    }
}

#[test]
fn auto_short_identifier_tokens_use_literal_scan() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::write(&source_file, "let id = 1;\nlet ab = id;\n").expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; the literal walk route
    // under test is taken while only the semantic lane is ready.
    install_ready_semantic_lane(&ctx, project.path());
    for query in ["id", "ab"] {
        let response = response_value(handle_semantic_search(&request(query), &ctx));

        assert_eq!(
            response["success"], true,
            "auto short-token query should succeed for {query:?}: {response:?}"
        );
        assert_ne!(response["interpreted_as"], "semantic", "query: {query:?}");
        assert_eq!(response["interpreted_as"], "literal", "query: {query:?}");
        assert_eq!(response["query_kind"], "Identifier", "query: {query:?}");
        assert!(
            response["results"]
                .as_array()
                .expect("results array")
                .iter()
                .any(|result| result["kind"] == "GrepLine" && result["match_text"] == query),
            "expected exact grep-line match for {query:?}: {response:?}"
        );
    }
}

#[test]
fn identifier_ready_reports_complete_success_with_semantic() {
    let (project, source_file, source) = project_with_needle();
    let (base_url, embedding_requests, handle) = start_no_request_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let mut identifier_request = request("needle_symbol");
    identifier_request.id = "identifier-ready-complete-with-semantic".to_string();
    let response = response_value(handle_semantic_search(&identifier_request, &ctx));
    handle.join().expect("negative embedding server thread");

    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["status"], "ready");
    assert_eq!(response["semantic_status"], "ready");
    assert_eq!(response["interpreted_as"], "hybrid");
    // `query` keeps the semantic lane for an identifier too: one embedding.
    assert_eq!(embedding_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        response["structuredContent"]["search"]["embedding_calls"],
        1
    );
    assert_eq!(
        response["structuredContent"]["search"]["embedding_cache_hits"],
        0
    );
    assert_eq!(
        response["structuredContent"]["search"]["live_embed_calls"],
        1
    );
}

/// Surrounding paired quotes in literal queries are stripped before matching.
/// Many agents and humans bring the GitHub-code-search / `rg -F "..."`
/// convention of quoting a phrase, and AFT's pure-substring matching would
/// otherwise silently return zero matches when the quotes are included.
#[test]
fn literal_query_strips_surrounding_paired_quotes() {
    let (project, _source_file, _) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());

    for (query, label) in [
        ("\"needle_symbol\"", "double-quoted"),
        ("'needle_symbol'", "single-quoted"),
    ] {
        let response = response_value(handle_semantic_search(
            &request_with(query, Some("literal")),
            &ctx,
        ));

        assert_eq!(
            response["success"], true,
            "{label} literal query should succeed after quote-strip: {response:?}"
        );
        // With a ready trigram lane the stripped literal runs on the lexical
        // lane rather than the no-index grep walk.
        assert_eq!(response["interpreted_as"], "lexical");
        assert_eq!(
            response["query"], "needle_symbol",
            "response query echo should reflect stripped form for {label} input"
        );
        assert!(
            response["results"]
                .as_array()
                .expect("results array")
                .iter()
                .any(|r| r["file"]
                    .as_str()
                    .is_some_and(|file| path_ends_with(file, "src/lib.rs"))),
            "stripped {label} query should match needle_symbol in source: {response:?}"
        );
    }
}

#[test]
fn literal_query_preserves_unmatched_quotes() {
    // Mixed quotes (`"foo'` / `'foo"`) are not a balanced outer pair — leave
    // them alone. Asymmetric stripping would be more confusing than the
    // matched-pair convention.
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::write(&source_file, "let s = \"'needle\";\n").expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());
    let response = response_value(handle_semantic_search(
        &request_with("\"'needle", Some("literal")),
        &ctx,
    ));

    assert_eq!(
        response["query"], "\"'needle",
        "unmatched quote should NOT be stripped"
    );
    assert_eq!(response["success"], true);
}

#[test]
fn quote_strip_does_not_produce_empty_query() {
    // `""` should be rejected as empty after stripping, not silently routed
    // into a wildcard search.
    let project = tempfile::tempdir().expect("create project dir");
    let ctx = test_context(project.path());

    for query in ["\"\"", "''"] {
        let response = response_value(handle_semantic_search(
            &request_with(query, Some("literal")),
            &ctx,
        ));
        assert_eq!(response["success"], false, "query {query:?} should fail");
        assert_eq!(response["code"], "invalid_request");
        assert_eq!(response["message"], "query must be non-empty");
    }
}

#[test]
fn quote_strip_only_removes_one_pair() {
    // Nested quotes: `""needle""` strips outer pair → `"needle"`, leaving the
    // inner quotes as part of the literal needle. Single-pair stripping
    // matches GitHub/grep convention and avoids overreach.
    let (project, source_file, _) = project_with_needle();
    std::fs::write(&source_file, "let s = \"needle\";\n").expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    // aft_search refuses when no index lane is ready; route over a ready
    // trigram lane.
    install_project_lexical_index(&ctx, project.path());
    let response = response_value(handle_semantic_search(
        &request_with("\"\"needle\"\"", Some("literal")),
        &ctx,
    ));

    assert_eq!(
        response["query"], "\"needle\"",
        "only outer pair should be stripped"
    );
    assert_eq!(response["success"], true);
}

#[test]
fn live_engine_pipeline_ranks_and_pages_with_provenance() {
    let project = tempfile::tempdir().expect("create project dir");
    let exact = project.path().join("src/z_exact.rs");
    let lexical = project.path().join("src/a_lexical.rs");
    let log = project.path().join("src/logging.rs");
    let path_match = project.path().join("src/subc_format.rs");
    std::fs::create_dir_all(exact.parent().unwrap()).expect("create src dir");
    let exact_source = "pub const MESSAGE: &str = \"needle symbol\";\n".to_string();
    let lexical_source =
        "needle needle needle\nfiller\nfiller\nfiller\nsymbol symbol symbol\n".to_string();
    let log_source = "error!(\"opening {} failed for run {}\", path, id);\n".to_string();
    let path_source = "pub fn subc_format() { callgraph_op(); }\n".to_string();
    std::fs::write(&exact, &exact_source).expect("write exact source");
    std::fs::write(&lexical, &lexical_source).expect("write lexical source");
    std::fs::write(&log, &log_source).expect("write log source");
    std::fs::write(&path_match, &path_source).expect("write path source");
    let ctx = test_context(project.path());
    install_lexical_index_entries(
        &ctx,
        &[
            (lexical.clone(), lexical_source),
            (exact.clone(), exact_source),
            (log.clone(), log_source),
            (path_match.clone(), path_source),
        ],
    );
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let first_request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "engine-live-first",
        "command": "semantic_search",
        "query": "needle symbol",
        "top_k": 1,
        "offset": 0
    }))
    .unwrap();
    let second_request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "engine-live-second",
        "command": "semantic_search",
        "query": "needle symbol",
        "top_k": 1,
        "offset": 1
    }))
    .unwrap();
    let first_raw = handle_semantic_search(&first_request, &ctx);
    let first_rendered = aft::subc_format::format_response("search", &first_raw, false);
    let first = response_value(first_raw);
    let second = response_value(handle_semantic_search(&second_request, &ctx));

    assert_eq!(first["success"], true, "first page failed: {first:?}");
    assert_eq!(second["success"], true, "second page failed: {second:?}");
    assert!(path_ends_with(
        first["results"][0]["file"].as_str().unwrap(),
        "src/z_exact.rs"
    ));
    assert_ne!(first["results"][0]["file"], second["results"][0]["file"]);
    assert_eq!(first["structuredContent"]["plan"]["exact_tier"], "e1");
    assert!(first["structuredContent"]["plan"]["confidence"].is_string());
    assert!(!first["structuredContent"]["results"][0]["lane_positions"]
        .as_object()
        .expect("lane provenance object")
        .is_empty());
    assert!(first_rendered.contains("narrow: offset, topK, path, includeTests"));

    let log_request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "engine-live-anchored",
        "command": "semantic_search",
        "query": "2026-09-08T12:00:01 ERROR opening /tmp/run-4821/a.rs failed for run 12345",
        "top_k": 1
    }))
    .unwrap();
    let log_response = response_value(handle_semantic_search(&log_request, &ctx));
    assert_eq!(
        log_response["success"], true,
        "anchored lane failed: {log_response:?}"
    );
    assert!(path_ends_with(
        log_response["results"][0]["file"].as_str().unwrap(),
        "src/logging.rs"
    ));
    assert_eq!(
        log_response["structuredContent"]["plan"]["exact_tier"],
        "anchored"
    );
    assert!(log_response["structuredContent"]["plan"]["lanes_run"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("anchored")));

    let path_request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "engine-live-path",
        "command": "semantic_search",
        "query": "subc_format.rs",
        "top_k": 1
    }))
    .unwrap();
    let path_response = response_value(handle_semantic_search(&path_request, &ctx));
    assert!(path_ends_with(
        path_response["results"][0]["file"].as_str().unwrap(),
        "src/subc_format.rs"
    ));
    assert_eq!(
        path_response["structuredContent"]["results"][0]["lane_positions"]["path_lookup"]
            ["disposition"],
        "depth_exempt"
    );
}

#[test]
fn external_borrowed_engine_exact_phrase_beats_sixty_dense_decoys() {
    const QUERY: &str = "settle refuses \"merged_ref is not integrated\": how is integration checked (against which ref: main, the campaign integration_ref, or the caller directory HEAD)";

    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let external = tempfile::tempdir().expect("external project");
    init_git(external.path());
    let src = external.path().join("crates/prefrontal-core-module/src");
    fs::create_dir_all(&src).expect("create external source tree");
    for index in 0..65 {
        let mut decoy = String::new();
        for _ in 0..30 {
            decoy.push_str(
                "merged_ref settle integrated refuses not is how integration checked against which ref main the campaign integration_ref or caller directory HEAD\n",
            );
        }
        fs::write(src.join(format!("decoy_{index:03}.rs")), decoy).expect("write dense decoy");
    }
    let target = src.join("worktree.rs");
    let mut target_source =
        format!("// {QUERY}\npub const INTEGRATION_FAILURE: &str = \"integration failed\";\n");
    for line in 0..5_000 {
        target_source.push_str(&format!("// unrelated specimen padding {line}\n"));
    }
    fs::write(&target, target_source).expect("write exact specimen");
    commit_all(external.path());

    let fixture_index = SearchIndex::build(external.path());
    let shape = aft::query_shape::classify(QUERY);
    let tokens = aft::query_shape::extract_lexical_tokens(QUERY, &shape);
    let token_refs = tokens.iter().map(String::as_str).collect::<Vec<_>>();
    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&token_refs);
    let fixture_snapshot = fixture_index.snapshot();
    let lexical_candidates =
        aft::commands::semantic_search::lexical_lane::CanonicalLexicalLane::from_snapshot(
            &fixture_snapshot,
            &query_trigrams,
            None,
            50,
        )
        .expect("enumerate fixture lexical lane");
    let specimen_lexical_rank = lexical_candidates
        .canonical_order()
        .iter()
        .position(|candidate| candidate.result.path.file_name() == target.file_name())
        .expect("specimen has lexical candidates");
    assert!(
        specimen_lexical_rank >= 50,
        "the exact specimen must remain outside the legacy candidate cap; rank was {specimen_lexical_rank}"
    );

    let in_root_ctx = test_context(external.path());
    *in_root_ctx
        .search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fixture_index);
    let in_root_response = response_value(handle_semantic_search(
        &request_with_top_k(QUERY, None, 5),
        &in_root_ctx,
    ));

    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external.path(), storage.path());
    let session = tempfile::tempdir().expect("session project");
    let ctx = test_context_with_storage(session.path(), storage.path());
    let raw_response =
        handle_semantic_search(&request_with_path(QUERY, None, external.path()), &ctx);
    let rendered = aft::subc_format::format_response("search", &raw_response, false);
    let response = response_value(raw_response);

    assert_eq!(
        response["success"], true,
        "external search failed: {response:?}"
    );
    assert_eq!(response["borrowed"], true);
    assert!(path_ends_with(
        response["results"][0]["file"]
            .as_str()
            .expect("rank-one file"),
        "crates/prefrontal-core-module/src/worktree.rs"
    ));
    assert_eq!(response["results"][0]["source"], "exact");
    let ranked_signature = |value: &Value| {
        value["results"]
            .as_array()
            .expect("ranked results")
            .iter()
            .map(|result| {
                (
                    result["file"]
                        .as_str()
                        .and_then(|path| path.rsplit('/').next())
                        .expect("ranked filename")
                        .to_string(),
                    result["source"]
                        .as_str()
                        .expect("result source")
                        .to_string(),
                    result["exact"].as_bool().expect("exact marker"),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ranked_signature(&response),
        ranked_signature(&in_root_response)
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.starts_with("shown "))
            .count(),
        1,
        "external engine reply must contain exactly one trailer: {rendered}"
    );
}

const EXTERNAL_IDENTIFIER: &str = "requireCredentialStamps";

/// Source files that each use `EXTERNAL_IDENTIFIER` once; `pool.ts` declares it.
const EXTERNAL_IDENTIFIER_SOURCES: [(&str, &str); 6] = [
    (
        "src/store/pool.ts",
        "export interface PoolOptions {\n  requireCredentialStamps?: boolean\n}\n",
    ),
    (
        "src/store/mutate.ts",
        "if (options.requireCredentialStamps) {\n  rejectUnstamped()\n}\n",
    ),
    (
        "src/store/runtime.ts",
        "const strict = config.requireCredentialStamps ?? false\n",
    ),
    (
        "src/store/schema.ts",
        "// requireCredentialStamps turns unbound rows into errors\n",
    ),
    (
        "src/store/torn.ts",
        "export function check(o) { return o.requireCredentialStamps }\n",
    ),
    (
        "src/guard.ts",
        "assert(opts.requireCredentialStamps === true)\n",
    ),
];

/// A Git project whose captured JSON dumps repeat the identifier's sub-tokens
/// on one giant line without ever spelling it. With `with_identifier` false the
/// source files hold a placeholder instead, so an index built now predates
/// the identifier.
fn external_identifier_project(with_identifier: bool) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("external project");
    let root = project.path();
    write_external_identifier_sources(root, with_identifier);
    fs::write(
        root.join("src/credentials.ts"),
        "// credential stamps are required before a row is used\nexport const requireStamps = true\n",
    )
    .expect("write related source");
    let dumps = root.join("research/evidence/dumps");
    fs::create_dir_all(&dumps).expect("create dumps dir");
    let body = format!(
        "{{\"model\":\"m\",\"instructions\":\"{}\"}}",
        "require credential stamps; the Credential store must require Stamps. ".repeat(3_000)
    );
    for index in 0..8 {
        fs::write(dumps.join(format!("{index:04}-main.body.json")), &body).expect("write dump");
    }
    init_git(root);
    commit_all(root);
    project
}

fn write_external_identifier_sources(root: &Path, with_identifier: bool) {
    for (relative, content) in EXTERNAL_IDENTIFIER_SOURCES {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("source parent")).expect("create source dir");
        let content = if with_identifier {
            content.to_string()
        } else {
            content.replace(EXTERNAL_IDENTIFIER, "placeholderOption")
        };
        fs::write(path, content).expect("write source");
    }
}

fn external_identifier_request(root: &Path) -> RawRequest {
    serde_json::from_value(serde_json::json!({
        "id": "aft-search-contract",
        "command": "semantic_search",
        "query": EXTERNAL_IDENTIFIER,
        "top_k": 25,
        "path": root.display().to_string(),
    }))
    .expect("build external identifier request")
}

fn result_suffixes(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|result| {
            result["file"]
                .as_str()
                .expect("result file")
                .replace('\\', "/")
        })
        .collect()
}

fn assert_every_source_use_first_and_no_dump(response: &Value) {
    let files = result_suffixes(response);
    assert!(
        files.len() >= 6,
        "every source use must come back: {response:#?}"
    );
    assert!(
        files[0].ends_with("src/store/pool.ts"),
        "the declaring file leads: {files:#?}"
    );
    for (relative, _) in EXTERNAL_IDENTIFIER_SOURCES {
        assert!(
            files[..6].iter().any(|file| file.ends_with(relative)),
            "{relative} must be among the first six results: {files:#?}"
        );
    }
    assert!(
        !files
            .iter()
            .any(|file| file.contains("/dumps/") || file.ends_with(".json")),
        "no dump file may pad an identifier query: {files:#?}"
    );
    assert!(
        response["results"]
            .as_array()
            .expect("results")
            .iter()
            .all(|result| result["source"] == "exact"),
        "only exact occurrences are listed: {response:#?}"
    );
}

#[test]
fn external_identifier_without_index_returns_every_source_use_and_no_dump() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let external = external_identifier_project(true);
    let session = tempfile::tempdir().expect("session project");
    let storage = tempfile::tempdir().expect("storage");
    let ctx = test_context_with_storage(session.path(), storage.path());

    let response = response_value(handle_semantic_search(
        &external_identifier_request(external.path()),
        &ctx,
    ));

    assert_eq!(response["success"], true, "{response:#?}");
    assert_eq!(response["semantic_status"], "external_unindexed");
    assert_every_source_use_first_and_no_dump(&response);
    assert_eq!(response["result_count"], 6);
    assert_eq!(response["complete"], true);
    assert_eq!(
        response["fully_degraded"], false,
        "a sweep that read every file is not a degraded answer"
    );
    assert_eq!(response["exact_sweep"]["complete"], true);
    assert_eq!(response["exact_sweep"]["used_index"], false);
    assert!(
        response["exact_sweep"]["coverage"]
            .as_str()
            .is_some_and(|coverage| coverage.starts_with("exact pass: complete; checked all")),
        "the summary carries the coverage sentence for JSON renderers: {response:#?}"
    );
    let text = response["text"].as_str().expect("text");
    assert!(
        text.contains("exact pass: complete; checked all"),
        "a finished walk says it covered the project: {text}"
    );
    assert!(
        !text.contains("use grep for an exhaustive check"),
        "a finished walk does not send the agent to grep: {text}"
    );
    assert!(
        response.get("results_list_envelope").is_none(),
        "a complete, uncut list carries no trailer: {response:#?}"
    );
}

#[test]
fn external_identifier_with_stale_borrowed_index_returns_every_source_use_and_no_dump() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let external = external_identifier_project(false);
    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external.path(), storage.path());
    // The identifier arrives after the project's own index was written, as it
    // does when nobody has opened a session in that project since.
    std::thread::sleep(Duration::from_millis(20));
    write_external_identifier_sources(external.path(), true);
    let session = tempfile::tempdir().expect("session project");
    let ctx = test_context_with_storage(session.path(), storage.path());

    let response = response_value(handle_semantic_search(
        &external_identifier_request(external.path()),
        &ctx,
    ));

    assert_eq!(response["success"], true, "{response:#?}");
    assert_eq!(response["borrowed"], true);
    assert_every_source_use_first_and_no_dump(&response);
    assert_eq!(response["result_count"], 6);
    assert_eq!(response["exact_sweep"]["used_index"], true);
    assert_eq!(response["exact_sweep"]["complete"], true);
    assert_eq!(
        response["complete"], true,
        "the identifier plan has no semantic lane, so a full sweep is complete"
    );
    assert!(
        response.get("saved_index_unverified").is_none(),
        "a sweep that read every changed file needs no unchecked-index notice"
    );
    assert!(
        response["exact_sweep"]["index_answered_files"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "unchanged files are answered by the project's own index: {response:#?}"
    );
    let text = response["text"].as_str().expect("text");
    assert!(
        text.contains("this project's own AFT index answered"),
        "the reply says when the project's own index was used: {text}"
    );
}

/// The saved index was written before a file existed. A prose query that only
/// that file answers must still find it: the saved index is compared with the
/// disk first, and the new file is read into the copy that answers.
#[test]
fn external_prose_query_finds_a_file_added_after_the_saved_index() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, _source_file, _source) = git_project_with_needle();
    let storage = tempfile::tempdir().expect("storage");
    persist_search_index(external_project.path(), storage.path());
    let added = external_project.path().join("src/quokkalith.rs");
    fs::write(
        &added,
        "// Assemble the zephyrine quokkalith from its parts.\npub fn assemble_zephyrine_quokkalith() {}\n",
    )
    .expect("write file added after the index was saved");
    let session = tempfile::tempdir().expect("session project");
    let ctx = test_context_with_storage(session.path(), storage.path());

    let response = response_value(handle_semantic_search(
        &request_with_path(
            "where is the zephyrine quokkalith assembled",
            None,
            external_project.path(),
        ),
        &ctx,
    ));

    assert_eq!(response["success"], true, "{response:#?}");
    assert_eq!(response["borrowed"], true);
    assert!(
        result_suffixes(&response)
            .iter()
            .any(|file| file.ends_with("src/quokkalith.rs")),
        "the file added after the index was saved must be found: {response:#?}"
    );
    assert_eq!(response["saved_index_check"]["complete"], true);
    assert_eq!(response["saved_index_check"]["added"], 1);
    assert!(
        response.get("saved_index_unverified").is_none(),
        "a saved index compared with every file needs no unchecked notice: {response:#?}"
    );
    let text = response["text"].as_str().expect("text");
    assert!(
        text.contains("Checked the saved AFT index of")
            && text.contains("since it was saved 1 file was added"),
        "the reply says the index was checked and what changed: {text}"
    );
}
