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
        Config {
            project_root: Some(project_root.to_path_buf()),
            ..Config::default()
        },
    )
}

fn test_context_with_storage(project_root: &Path, storage_dir: &Path) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project_root.to_path_buf()),
            storage_dir: Some(storage_dir.to_path_buf()),
            ..Config::default()
        },
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
        },
    );
}

fn openai_context(project_root: &Path, base_url: String) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
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
            },
            ..Config::default()
        },
    )
}

fn openai_context_with_storage(
    project_root: &Path,
    storage_dir: &Path,
    base_url: String,
) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
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
            },
            ..Config::default()
        },
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
    // Honesty: the trigram lexical lane produced these results; semantic never
    // ran. interpreted_as must report what executed ("lexical"), not the routed
    // hybrid mode that was attempted.
    assert_eq!(response["interpreted_as"], "lexical");
    assert_eq!(response["status"], "ready");
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| result["source"] == "lexical"
            && result["file"]
                .as_str()
                .is_some_and(|file| path_ends_with(file, "src/lib.rs"))),
        "expected lexical fallback result, got {results:?}"
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
fn natural_language_auto_falls_back_to_grep_when_semantic_disabled() {
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
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request("how retry logic works"),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "natural-language fallback should succeed: {response:?}"
    );
    assert_eq!(response["query_kind"], "NaturalLanguage");
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["lexical_only_fallback"], true);
    assert!(
        response["results"]
            .as_array()
            .expect("results array")
            .iter()
            .any(|result| result["kind"] == "GrepLine"
                && result["line_text"]
                    .as_str()
                    .is_some_and(|line| line.contains("how retry logic works"))),
        "expected literal degraded fallback result: {response:?}"
    );
}

#[test]
fn degraded_grep_reports_file_cap_gap_when_scan_limit_reached() {
    let project = tempfile::tempdir().expect("create project dir");
    let src_dir = project.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create source dir");
    for index in 0..=1_000 {
        std::fs::write(
            src_dir.join(format!("module_{index}.rs")),
            format!("pub fn unrelated_{index}() {{}}\n"),
        )
        .expect("write source file");
    }

    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request("how slow backend fallback works"),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "degraded grep fallback should succeed: {response:?}"
    );
    assert_eq!(response["complete"], false);
    assert_eq!(response["fully_degraded"], true);
    assert_eq!(response["engine_capped"], true);
    assert_eq!(response["more_available"], true);
    assert_eq!(response["result_count"], 0);
    assert_eq!(response["degraded_grep_walk_truncated"], true);
    assert_eq!(response["degraded_grep_file_limit"], 1_000);
    assert_eq!(response["degraded_grep_candidate_files"], 1_000);

    let warnings = response["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("1000-file scan cap"))),
        "expected degraded grep file cap warning, got {warnings:?}"
    );
}

#[test]
fn natural_language_auto_falls_back_to_grep_while_semantic_builds() {
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

    assert_eq!(
        response["success"], true,
        "natural-language building fallback should succeed: {response:?}"
    );
    assert_eq!(response["query_kind"], "NaturalLanguage");
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(response["semantic_status"], "building");
    assert_eq!(response["lexical_only_fallback"], true);
    assert!(
        response["results"]
            .as_array()
            .expect("results array")
            .iter()
            .any(|result| result["kind"] == "GrepLine"
                && result["line_text"]
                    .as_str()
                    .is_some_and(|line| line.contains("how retry logic works"))),
        "expected literal degraded fallback result while building: {response:?}"
    );
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

    let response = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), external_project.path()),
        &ctx,
    ));

    // An unindexed foreign root must degrade to a bounded lexical scan with a
    // disclosure — not dead-end with a not_indexed error (which pushes agents
    // to shell out to grep/bash instead of staying on aft_search).
    assert_eq!(response["success"], true, "expected success: {response:?}");
    assert_eq!(response["fully_degraded"], true);
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
        Config {
            project_root: Some(session_project.path().to_path_buf()),
            restrict_to_project_root: true,
            ..Config::default()
        },
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
        &request_with_path("needle_symbol", Some("semantic"), external_project.path()),
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

    let response = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", None, external_project.path()),
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
        results.iter().any(|result| result["source"] == "lexical"
            && result["file"].as_str().is_some_and(|file| {
                Path::new(file).is_absolute() && file.replace('\\', "/").ends_with("src/lib.rs")
            })),
        "expected absolute lexical result from external project: {response:?}"
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
fn auto_mode_falls_back_to_grep_when_trigram_unavailable_and_semantic_disabled() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_degraded_grep_fallback(&response, "disabled");
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
    let semantic_request = request_with("needle_symbol", Some("semantic"));
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
fn legacy_semantic_hint_is_ignored_without_index() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("semantic")),
        &ctx,
    ));

    assert_eq!(response["success"], true);
    assert!(response["text"]
        .as_str()
        .expect("fallback text")
        .contains("lexical-only fallback"));
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

    for (query, hint, expected_source) in [
        ("^pub fn exported", "regex", "regex"),
        ("needle_symbol", "literal", "literal"),
    ] {
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
fn degraded_grep_filters_test_support_files_unless_requested() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    let fixture_file = project.path().join("fixtures/notes.txt");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    std::fs::create_dir_all(fixture_file.parent().expect("fixture parent"))
        .expect("create fixture dir");
    std::fs::write(&fixture_file, "how retry schema fallback works\n").expect("write fixture file");
    std::fs::write(
        &source_file,
        "pub fn retry() { /* how retry schema fallback works */ }\n",
    )
    .expect("write source file");
    let ctx = test_context(project.path());
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let default_response = response_value(handle_semantic_search(
        &request("how retry schema fallback works"),
        &ctx,
    ));
    assert_eq!(
        default_response["success"], true,
        "default fallback should succeed"
    );
    let default_results = default_response["results"]
        .as_array()
        .expect("results array");
    assert!(
        default_results.iter().all(|result| result["file"]
            .as_str()
            .is_some_and(|file| !file.replace('\\', "/").contains("/fixtures/"))),
        "default degraded grep should hide fixtures: {default_response:?}"
    );

    let include_response = response_value(handle_semantic_search(
        &request_with_include_tests("how retry schema fallback works", None, 5, true),
        &ctx,
    ));
    assert_eq!(
        include_response["success"], true,
        "include_tests fallback should succeed"
    );
    let include_results = include_response["results"]
        .as_array()
        .expect("results array");
    assert!(
        include_results.iter().any(|result| result["file"]
            .as_str()
            .is_some_and(|file| file.replace('\\', "/").ends_with("fixtures/notes.txt"))),
        "include_tests:true should surface degraded fixtures: {include_response:?}"
    );
}

#[test]
fn hybrid_semantic_results_report_semantic_source_and_boost_metadata() {
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
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

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
            matches!(source, "semantic" | "lexical"),
            "hybrid response result source must be semantic or lexical, got {source:?}: {result:?}"
        );
        assert_ne!(source, "hybrid");
    }

    let boosted = results
        .iter()
        .find(|result| result["source"] == "semantic" && result["lexical_score"].is_number())
        .expect("semantic result should carry separate lexical boost metadata");
    assert_eq!(boosted["hybrid_boosted"], true);
    assert!(boosted.get("hybrid_boosted").is_some());

    handle.join().expect("embedding server thread");
}

#[test]
fn lexical_only_fallback_reports_more_available_when_capped_or_over_top_k() {
    let (project, entries) = project_with_repeated_needle_files(6);
    let ctx = test_context(project.path());
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with_top_k("needle_symbol", None, 5),
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
        &request_with_top_k("needle_symbol", None, 100),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "building lexical fallback should succeed: {response:?}"
    );
    assert_eq!(response["status"], "building");
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(response["engine_capped"], true);
    assert_eq!(response["more_available"], true);
}

#[test]
fn hybrid_ready_reports_more_available_when_lexical_engine_capped() {
    let (project, entries) = project_with_repeated_needle_files(210);
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index_entries(&ctx, &entries);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let response = response_value(handle_semantic_search(
        &request_with_top_k("needle_symbol", None, 100),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "ready hybrid query should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["complete"], true);
    assert_eq!(response["interpreted_as"], "hybrid");
    assert_eq!(response["engine_capped"], true);
    assert_eq!(response["more_available"], true);
    handle.join().expect("embedding server thread");
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
        &request_with_top_k("needle_symbol", Some("semantic"), 100),
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
fn hybrid_ready_reports_no_more_available_when_under_top_k_without_caps() {
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
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let response = response_value(handle_semantic_search(
        &request_with_top_k("needle_symbol", None, 5),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "ready hybrid query should succeed: {response:?}"
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
    handle.join().expect("embedding server thread");
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
fn hybrid_ready_semantic_reports_complete_success() {
    let (project, source_file, source) = project_with_needle();
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["status"], "ready");
    assert_eq!(response["semantic_status"], "ready");
    assert_eq!(response["interpreted_as"], "hybrid");
    handle.join().expect("embedding server thread");
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
        assert_eq!(response["interpreted_as"], "literal");
        assert_eq!(
            response["query"], "needle_symbol",
            "response query echo should reflect stripped form for {label} input"
        );
        assert!(
            response["results"]
                .as_array()
                .expect("results array")
                .iter()
                .any(|r| r["kind"] == "GrepLine"),
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
