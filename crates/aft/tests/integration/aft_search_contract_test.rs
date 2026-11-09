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
use aft::search_index::artifact_cache_key;
use aft::semantic_index::SemanticIndex;
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
    .with_test_checkpoint_store()
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
    .with_test_checkpoint_store()
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
    .with_test_checkpoint_store()
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

fn start_mock_embedding_server() -> (String, thread::JoinHandle<()>) {
    let (base_url, _, handle) = start_counted_mock_embedding_server();
    (base_url, handle)
}

fn start_counted_mock_embedding_server() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    start_mock_embedding_server_with_response(
        "200 OK",
        r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#,
    )
}

fn start_mock_embedding_error_server() -> (String, thread::JoinHandle<()>) {
    let (base_url, _, handle) = start_mock_embedding_server_with_response(
        "400 Bad Request",
        r#"{"error":"embedding boom"}"#,
    );
    (base_url, handle)
}

fn start_mock_embedding_server_with_response(
    status: &str,
    body: &str,
) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let addr = listener.local_addr().expect("embedding server addr");
    let status = status.to_string();
    let body = body.to_string();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_thread = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        // Setup may take arbitrarily long before search embeds its query. Keep
        // the listener alive until that request, and drain its body before
        // replying so a split HTTP write cannot be reset by an early close.
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

        requests_for_thread.fetch_add(1, Ordering::SeqCst);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    });

    (format!("http://{addr}"), requests, handle)
}

fn path_ends_with(file: &str, suffix: &str) -> bool {
    file.replace('\\', "/").ends_with(suffix)
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
    )
    .with_test_checkpoint_store();
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
fn external_plain_folder_returns_unindexed_scan_hits_without_indexing() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let session_project = tempfile::tempdir().expect("session project");
    let (external_project, _, _) = project_with_needle();
    let storage = tempfile::tempdir().expect("storage");
    let ctx = test_context_with_storage(session_project.path(), storage.path());

    for query in ["needle_symbol", "fn needle_symbol"] {
        let response = response_value(handle_semantic_search(
            &request_with_path(query, Some("literal"), external_project.path()),
            &ctx,
        ));

        assert_eq!(response["success"], true, "{response:?}");
        assert_eq!(response["semantic_status"], "external_unindexed");
        let results = response["results"].as_array().expect("results");
        assert_eq!(results.len(), 1, "{response:?}");
        assert!(path_ends_with(
            results[0]["file"].as_str().expect("file"),
            "src/lib.rs"
        ));
        let text = response["text"].as_str().expect("text");
        assert!(text.to_ascii_lowercase().contains("no aft index"), "{text}");
    }
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
    assert!(!external_project.path().join(".git").exists());
    assert!(!external_project.path().join(".aft").exists());
}

#[test]
fn external_plain_folder_honours_gitignore_and_aftignore() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let session_project = tempfile::tempdir().expect("session project");
    let (external_project, _, _) = project_with_needle();
    for (directory, ignore_name, ignored) in [
        ("", ".gitignore", "git_hidden.rs"),
        ("", ".aftignore", "aft_hidden.rs"),
        ("nested", ".gitignore", "nested_git_hidden.rs"),
        ("nested", ".aftignore", "nested_aft_hidden.rs"),
    ] {
        let directory = external_project.path().join(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(ignore_name), format!("{ignored}\n")).unwrap();
        fs::write(directory.join(ignored), "pub fn needle_symbol() {}\n").unwrap();
    }
    let ctx = test_context(session_project.path());
    for query in ["needle_symbol", "fn needle_symbol"] {
        let response = response_value(handle_semantic_search(
            &request_with_path(query, Some("literal"), external_project.path()),
            &ctx,
        ));
        assert_eq!(response["success"], true, "{response:?}");
        let results = response["results"].as_array().expect("results");
        assert_eq!(
            results.len(),
            1,
            "ignored files must not appear: {response:?}"
        );
        assert!(path_ends_with(
            results[0]["file"].as_str().expect("file"),
            "src/lib.rs"
        ));
    }
}

#[test]
fn external_plain_folder_respects_configured_and_force_restriction() {
    let session_project = tempfile::tempdir().expect("session project");
    let (external_project, _, _) = project_with_needle();
    let ctx = test_context(session_project.path());
    for configured in [true, false] {
        ctx.update_config(|config| config.restrict_to_project_root = configured);
        let _guard = (!configured).then(|| ctx.force_restrict_guard("aft-search-contract"));
        let response = response_value(handle_semantic_search(
            &request_with_path("needle_symbol", Some("literal"), external_project.path()),
            &ctx,
        ));
        assert_eq!(response["success"], false, "{response:?}");
        assert_eq!(response["code"], "path_outside_root", "{response:?}");
    }
}

#[test]
fn external_volume_root_is_refused_with_grep_hint() {
    let session_project = tempfile::tempdir().expect("session project");
    let ctx = test_context(session_project.path());
    // Use filesystem root rather than ambient HOME so this public search response
    // stays stable when tests isolate HOME or the host home is a Git checkout.
    // readonly_artifacts.rs tests cover temporary environment-home and account-home
    // paths directly.
    let root = session_project
        .path()
        .ancestors()
        .last()
        .expect("filesystem root");
    let response = response_value(handle_semantic_search(
        &request_with_path("needle_symbol", Some("literal"), root),
        &ctx,
    ));
    assert_eq!(response["success"], false, "{response:?}");
    assert_eq!(response["code"], "not_a_git_root", "{response:?}");
    let message = response["message"].as_str().expect("message");
    assert!(message.contains("bounded"), "{message}");
    assert!(message.contains("grep"), "{message}");
}

#[test]
fn external_path_obeys_force_restrict_guard() {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, _external_source, _source) = git_project_with_needle();
    let session_project = tempfile::tempdir().expect("session project");
    let storage = tempfile::tempdir().expect("storage");
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
fn external_hyphenated_literal_ranks_the_containing_file_first_and_only_it_is_exact() {
    // A path naming another Git project searches that project's persisted
    // (borrowed) index instead of the session's own, so the literal routing
    // must hold on that path too.
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    let (external_project, _entries) = project_with_hyphenated_literal();
    init_git(external_project.path());
    commit_all(external_project.path());
    let storage = tempfile::tempdir().expect("storage");
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

fn format_query_embedding_phases(phases: &[(&str, Instant)], started: Instant) -> String {
    [
        "search_start",
        "embed_query_entry",
        "model_mutex_first_acquired",
        "model_construction_start",
        "model_construction_end",
        "model_mutex_second_acquired",
        "model_mutex_reacquired",
        "request_send",
        "deadline_fire_observed",
        "fallback_start",
        "reply",
    ]
    .iter()
    .map(|phase| {
        let offsets = phases
            .iter()
            .filter(|(name, _)| name == phase)
            .map(|(_, at)| {
                format!(
                    "{:.3}ms",
                    at.duration_since(started).as_secs_f64() * 1_000.0
                )
            })
            .collect::<Vec<_>>();
        let offset = if offsets.is_empty() {
            "not reached".to_string()
        } else {
            offsets.join(" / ")
        };
        format!("{phase}={offset}")
    })
    .collect::<Vec<_>>()
    .join(", ")
}

#[test]
fn query_embedding_phase_report_includes_missing_and_repeated_phases() {
    let started = Instant::now();
    let report = format_query_embedding_phases(
        &[
            ("search_start", started),
            ("request_send", started + Duration::from_millis(10)),
            ("request_send", started + Duration::from_millis(20)),
            ("reply", started + Duration::from_millis(750)),
        ],
        started,
    );
    assert_eq!(report, "search_start=0.000ms, embed_query_entry=not reached, model_mutex_first_acquired=not reached, model_construction_start=not reached, model_construction_end=not reached, model_mutex_second_acquired=not reached, model_mutex_reacquired=not reached, request_send=10.000ms / 20.000ms, deadline_fire_observed=not reached, fallback_start=not reached, reply=750.000ms");
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

fn render_search(response: Response) -> (String, Value) {
    let rendered = aft::subc_format::format_response("search", &response, false);
    (rendered, response_value(response))
}

/// When the query embedding fails, the answer comes from a filesystem scan
/// and the reply says so; the lane footer must not then print
/// `semantic: ready`. Both lines come from the record of what served this
/// request.
#[test]
fn issue_826_lane_footer_agrees_with_a_degraded_answer() {
    let (project, _source_file, _source) = project_with_needle();
    let (base_url, handle) = start_mock_embedding_error_server();
    let ctx = openai_context(project.path(), base_url);
    install_ready_semantic_lane(&ctx, project.path());

    let (rendered, response) =
        render_search(handle_semantic_search(&request("needle_symbol"), &ctx));
    handle.join().expect("embedding server thread");

    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["fully_degraded"], true, "{response}");
    assert_eq!(response["semantic_unavailable"], true, "{response}");
    assert!(
        rendered.contains("Search status: answered by a filesystem scan"),
        "{rendered}"
    );
    assert!(
        !rendered.lines().any(|line| line == "semantic: ready"),
        "the semantic lane did not serve this answer, so the footer may not call it ready:\n{rendered}"
    );
    assert!(
        rendered.lines().any(|line| line
            == "semantic: unavailable for this query (query embedding failed: HTTP 400)"),
        "{rendered}"
    );
}
