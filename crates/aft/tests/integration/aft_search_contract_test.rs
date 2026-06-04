use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use aft::commands::semantic_search::handle_semantic_search;
use aft::config::{Config, SemanticBackend, SemanticBackendConfig};
use aft::context::{AppContext, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_index::SearchIndex;
use aft::semantic_index::SemanticIndex;
use serde_json::Value;

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
    *ctx.search_index().borrow_mut() = Some(index);
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
    *ctx.search_index().borrow_mut() = Some(index);
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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
        stage: "embedding".to_string(),
        files: Some(1),
        entries_done: Some(0),
        entries_total: Some(1),
        stats: None,
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
fn hybrid_disabled_semantic_uses_lexical_only_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_lexical_fallback(&response, "disabled");
}

#[test]
fn hybrid_failed_semantic_uses_lexical_only_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() =
        SemanticIndexStatus::Failed("ONNX Runtime unavailable".to_string());

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_lexical_fallback(&response, "unavailable");
}

#[test]
fn auto_mode_falls_back_to_grep_when_trigram_unavailable_and_semantic_disabled() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_degraded_grep_fallback(&response, "disabled");
}

#[test]
fn embed_query_failure_falls_back_to_grep_when_hint_not_explicit_semantic() {
    let (project, _source_file, _source) = project_with_needle();
    let (base_url, handle) = start_mock_embedding_error_server();
    let ctx = openai_context(project.path(), base_url);
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_degraded_grep_fallback(&response, "unavailable");
    handle.join().expect("embedding server thread");
}

#[test]
fn explicit_hint_semantic_still_fails_cleanly_when_no_fallback_available() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("semantic")),
        &ctx,
    ));

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "semantic_unavailable");
}

#[test]
fn explicit_semantic_hint_fails_when_semantic_is_unavailable() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("semantic")),
        &ctx,
    ));

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "semantic_unavailable");
}

#[test]
fn regex_grep_success_reports_ready_status_not_semantic_backend_status() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(semantic_index);

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
        stage: "embedding".to_string(),
        files: Some(210),
        entries_done: Some(0),
        entries_total: Some(210),
        stats: None,
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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(SemanticIndex::new(project.path().to_path_buf(), 3));

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(semantic_index);

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(semantic_index);

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(SemanticIndex::new(project.path().to_path_buf(), 3));

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
fn literal_query_strips_surrounding_double_quotes() {
    let (project, _source_file, _) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("\"needle_symbol\"", Some("literal")),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "quoted literal query should succeed after quote-strip: {response:?}"
    );
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(
        response["query"], "needle_symbol",
        "response query echo should reflect stripped form"
    );
    assert!(
        response["results"]
            .as_array()
            .expect("results array")
            .iter()
            .any(|r| r["kind"] == "GrepLine"),
        "stripped query should match needle_symbol in source: {response:?}"
    );
}

#[test]
fn literal_query_strips_surrounding_single_quotes() {
    let (project, _source_file, _) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("'needle_symbol'", Some("literal")),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "single-quoted literal query should succeed: {response:?}"
    );
    assert_eq!(response["query"], "needle_symbol");
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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
