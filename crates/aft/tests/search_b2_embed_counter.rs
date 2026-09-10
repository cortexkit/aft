use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use aft::commands::semantic_search::handle_semantic_search;
use aft::config::{Config, SemanticBackend, SemanticBackendConfig};
use aft::context::{AppContext, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_b2::embed_counter::{install, read, EmbedCounts, FIXTURE_PROVIDER_MODEL};
use aft::semantic_index::SemanticIndex;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct CounterCases {
    zero_embedding_queries: Vec<String>,
    semantic_controls: Vec<String>,
}

fn cases() -> CounterCases {
    serde_json::from_str(include_str!("fixtures/search_b2/embed_counter/cases.json"))
        .expect("parse embedding-counter fixtures")
}

fn request(id: &str, query: &str) -> RawRequest {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "command": "semantic_search",
        "query": query,
        "top_k": 5
    }))
    .expect("build search request")
}

fn response_value(response: Response) -> Value {
    serde_json::to_value(response).expect("serialize search response")
}

fn response_counts(response: &Value) -> EmbedCounts {
    let search = &response["structuredContent"]["search"];
    EmbedCounts {
        requested: search["embedding_calls"]
            .as_u64()
            .expect("embedding_calls counter"),
        cache_hits: search["embedding_cache_hits"]
            .as_u64()
            .expect("embedding_cache_hits counter"),
        live_calls: search["live_embed_calls"]
            .as_u64()
            .expect("live_embed_calls counter"),
    }
}

fn semantic_lane_ran(response: &Value) -> bool {
    let plan = &response["structuredContent"]["plan"];
    plan["lanes_run"]
        .as_array()
        .or_else(|| plan["selected_lanes"].as_array())
        .is_some_and(|lanes| lanes.iter().any(|lane| lane == "semantic"))
}

fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = tempfile::tempdir().expect("create project");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source directory");
    std::fs::write(
        &source_file,
        "pub fn parse_header() -> &'static str { \"semantic embedding cache\" }\n",
    )
    .expect("write source fixture");
    (project, source_file)
}

fn context_with_semantic_index(
    project_root: &Path,
    source_file: &Path,
    base_url: String,
    model: &str,
) -> AppContext {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        semantic: SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: model.to_string(),
            base_url: Some(base_url),
            api_key_env: None,
            timeout_ms: 5_000,
            query_timeout_ms: 3_000,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        },
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index = SemanticIndex::build(
        project_root,
        std::slice::from_ref(&source_file.to_path_buf()),
        &mut embed,
        16,
    )
    .expect("build semantic index");
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);
    ctx
}

fn start_embedding_server(expected_calls: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let address = listener.local_addr().expect("embedding server address");
    let handle = thread::spawn(move || {
        for _ in 0..expected_calls {
            let (mut stream, _) = listener.accept().expect("accept embedding request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read embedding request");
            let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write embedding response");
        }
    });
    (format!("http://{address}"), handle)
}

#[test]
fn non_semantic_public_rows_never_reach_the_embedding_boundary() {
    let (project, _) = project();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().to_path_buf()),
            ..Config::default()
        },
    );

    for (index, query) in cases().zero_embedding_queries.iter().enumerate() {
        for pass in ["cold", "warm"] {
            let id = format!("counter-zero-{index}-{pass}");
            let response = response_value(handle_semantic_search(&request(&id, query), &ctx));
            assert_eq!(response["success"], true, "query {query:?}: {response:?}");
            assert_eq!(
                response_counts(&response),
                EmbedCounts::default(),
                "query {query:?} must not look up or request an embedding on {pass}"
            );
        }
    }
}

#[test]
fn semantic_short_and_natural_language_rows_report_cold_and_warm_observations() {
    for (index, query) in cases().semantic_controls.iter().enumerate() {
        let (project, source_file) = project();
        let (base_url, server) = start_embedding_server(1);
        let ctx = context_with_semantic_index(
            project.path(),
            &source_file,
            base_url,
            FIXTURE_PROVIDER_MODEL,
        );

        let cold = response_value(handle_semantic_search(
            &request(&format!("counter-semantic-{index}-cold"), query),
            &ctx,
        ));
        let warm = response_value(handle_semantic_search(
            &request(&format!("counter-semantic-{index}-warm"), query),
            &ctx,
        ));

        assert_eq!(cold["success"], true, "cold query failed: {cold:?}");
        assert_eq!(warm["success"], true, "warm query failed: {warm:?}");
        assert!(semantic_lane_ran(&cold));
        assert!(semantic_lane_ran(&warm));
        assert_eq!(
            response_counts(&cold),
            EmbedCounts {
                requested: 1,
                cache_hits: 0,
                live_calls: 0,
            }
        );
        assert_eq!(
            response_counts(&warm),
            EmbedCounts {
                requested: 0,
                cache_hits: 1,
                live_calls: 0,
            }
        );
        server.join().expect("embedding server");
    }
}

#[test]
fn fixture_and_live_providers_have_distinct_live_call_counts() {
    for (index, (model, expected_live_calls)) in
        [(FIXTURE_PROVIDER_MODEL, 0), ("production-model", 1)]
            .into_iter()
            .enumerate()
    {
        let (project, source_file) = project();
        let (base_url, server) = start_embedding_server(1);
        let ctx = context_with_semantic_index(project.path(), &source_file, base_url, model);
        let response = response_value(handle_semantic_search(
            &request(
                &format!("counter-provider-{index}"),
                "where does semantic embedding find this function",
            ),
            &ctx,
        ));

        assert_eq!(response["success"], true, "provider query: {response:?}");
        assert_eq!(response_counts(&response).requested, 1);
        assert_eq!(
            response_counts(&response).live_calls,
            expected_live_calls,
            "provider model {model}"
        );
        server.join().expect("embedding server");
    }
}

#[test]
fn concurrent_public_requests_keep_attribution_isolated() {
    let workers = (0..2)
        .map(|index| {
            thread::spawn(move || {
                let (project, source_file) = project();
                let (base_url, server) = start_embedding_server(1);
                let ctx = context_with_semantic_index(
                    project.path(),
                    &source_file,
                    base_url,
                    FIXTURE_PROVIDER_MODEL,
                );
                let response = response_value(handle_semantic_search(
                    &request(
                        &format!("counter-concurrent-{index}"),
                        "where does semantic embedding find this function",
                    ),
                    &ctx,
                ));
                server.join().expect("embedding server");
                response_counts(&response)
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        assert_eq!(
            worker.join().expect("request worker"),
            EmbedCounts {
                requested: 1,
                cache_hits: 0,
                live_calls: 0,
            }
        );
    }
}

#[test]
fn unattributed_index_builds_record_nothing() {
    let (project, source_file) = project();
    let request_id = "counter-unattributed-build";
    let _guard = install(request_id);
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);

    SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&source_file),
        &mut embed,
        16,
    )
    .expect("build semantic index");

    assert_eq!(read(request_id), EmbedCounts::default());
}

#[test]
fn b2_path_fence_rejects_second_semantic_index_record_site() {
    let source = include_str!("../src/semantic_index.rs");
    let embed_query_start = source
        .find("pub fn embed_query_cached")
        .expect("embed_query_cached definition");
    let embed_texts_start = source
        .find("fn embed_texts")
        .expect("embed_texts definition");
    let before_embed_texts = &source[embed_query_start..embed_texts_start];
    let embed_texts = &source[embed_texts_start..];

    assert!(
        !before_embed_texts.contains("query_embedding_cache.get"),
        "query cache resolution must not return before embed_texts"
    );
    assert_eq!(
        source.matches("embed_counter::record(").count(),
        1,
        "semantic_index.rs must contain exactly one B2 recording site"
    );
    let cache_resolution = embed_texts
        .find("query_embedding_cache.get")
        .expect("query cache resolution inside embed_texts");
    let recording_site = embed_texts
        .find("embed_counter::record(")
        .expect("embedding recording site");
    let warm_return = embed_texts
        .find("return Ok(vectors)")
        .expect("warm-cache return");
    assert!(cache_resolution < recording_site);
    assert!(recording_site < warm_return);
}
