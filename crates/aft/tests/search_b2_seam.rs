use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::SystemTime;

use aft::commands::semantic_search::handle_semantic_search;
use aft::config::{Config, SemanticBackend, SemanticBackendConfig};
use aft::context::{AppContext, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_b2::embed_counter::FIXTURE_PROVIDER_MODEL;
use aft::search_index::SearchIndex;
use aft::semantic_index::SemanticIndex;
use serde_json::Value;

fn request(id: &str, query: &str) -> RawRequest {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "command": "semantic_search",
        "query": query,
        "top_k": 5
    }))
    .expect("build request")
}

fn response_value(response: Response) -> Value {
    serde_json::to_value(response).expect("serialize response")
}

fn start_embedding_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let address = listener.local_addr().expect("embedding server address");
    let handle = thread::spawn(move || {
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
    });
    (format!("http://{address}"), handle)
}

fn all_ready_context(base_url: String) -> (tempfile::TempDir, AppContext) {
    let project = tempfile::tempdir().expect("create project");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source directory");
    let source = "pub fn character_cap() -> usize { 42 }\n";
    std::fs::write(&source_file, source).expect("write source fixture");

    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().to_path_buf()),
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: FIXTURE_PROVIDER_MODEL.to_string(),
                base_url: Some(base_url),
                api_key_env: None,
                timeout_ms: 5_000,
                query_timeout_ms: 3_000,
                max_batch_size: 64,
                max_files: 20_000,
                ..Default::default()
            },
            ..Config::default()
        },
    );

    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&source_file),
        &mut embed,
        16,
    )
    .expect("build semantic fixture");
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);

    let search_index = SearchIndex::build(project.path());
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(search_index);

    let metadata = std::fs::metadata(&source_file).expect("source metadata");
    ctx.symbol_cache()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            source_file,
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            metadata.len(),
            blake3::hash(source.as_bytes()),
            Vec::new(),
        );

    (project, ctx)
}

#[test]
fn public_all_ready_short_request_executes_each_selected_callback_once() {
    let (base_url, server) = start_embedding_server();
    let (_project, ctx) = all_ready_context(base_url);
    let response = response_value(handle_semantic_search(
        &request("seam-all-ready-short", "character cap"),
        &ctx,
    ));
    server.join().expect("embedding server");

    assert_eq!(response["success"], true, "{response:?}");
    let plan = &response["structuredContent"]["plan"];
    let selected = serde_json::json!(["symbol", "exact", "lexical", "variants", "semantic"]);
    assert_eq!(plan["shape"], "short");
    assert_eq!(plan["lanes_run"], selected);
    assert_eq!(plan["executed_callbacks"], selected);
    assert_eq!(
        plan["callback_counts"],
        serde_json::json!({
            "symbol": 1,
            "exact": 1,
            "lexical": 1,
            "variants": 1,
            "semantic": 1
        })
    );
    assert_eq!(plan["embedding_calls"], 1);
    assert_eq!(plan["embedding_cache_hits"], 0);
    assert_eq!(plan["live_embed_calls"], 0);
}

#[test]
fn readiness_disclosure_is_a_callback_but_never_a_retrieval_lane() {
    use aft::commands::semantic_search::extensions::{
        RawQuery, Readiness, RetainedReadinessSnapshots,
    };
    use aft::commands::semantic_search::plan_table::SearchLaneKind;
    use aft::search_b2::install_defaults;

    let raw_query = RawQuery::new("parse_header");
    let (shape, facts) = install_defaults().classify(&raw_query);
    let readiness = Readiness::observed(
        false,
        true,
        true,
        vec!["symbol:disabled".to_string()],
        RetainedReadinessSnapshots::default(),
        false,
    );
    let plan = install_defaults().plan(&shape, &facts, &readiness);
    assert_eq!(
        plan.selected_lanes,
        [SearchLaneKind::Lexical, SearchLaneKind::Variants]
    );
    assert_eq!(
        plan.executed_callbacks,
        [
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::ReadinessDisclosure
        ]
    );
    assert!(!plan
        .selected_lanes
        .contains(&SearchLaneKind::ReadinessDisclosure));
}

#[test]
fn every_retrieval_callback_has_a_legal_selecting_request() {
    use aft::commands::semantic_search::extensions::{RawQuery, Readiness};
    use aft::commands::semantic_search::plan_table::SearchLaneKind;
    use aft::search_b2::install_defaults;

    let extensions = install_defaults();
    let ready = Readiness::new(true, true, true);
    let mut selected = std::collections::BTreeSet::new();
    for query in [
        "parse_header",
        "character cap",
        "2026-09-08 ERROR worker failed",
        "where do we cap the room name",
        "src/main.rs",
    ] {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = extensions.classify(&raw_query);
        selected.extend(extensions.plan(&shape, &facts, &ready).selected_lanes);
    }
    let all_down = Readiness::new(false, false, false);
    let raw_query = RawQuery::new("parse_header");
    let (shape, facts) = extensions.classify(&raw_query);
    selected.extend(extensions.plan(&shape, &facts, &all_down).selected_lanes);

    assert_eq!(
        selected,
        SearchLaneKind::ALL
            .into_iter()
            .filter(|lane| *lane != SearchLaneKind::ReadinessDisclosure)
            .collect::<std::collections::BTreeSet<_>>()
    );
}

#[test]
fn public_log_and_path_requests_focus_anchored_and_path_lookup_callbacks() {
    let (_project, ctx) = all_ready_context("http://127.0.0.1:9".to_string());
    for (index, (query, expected)) in [
        (
            "2026-09-08 ERROR worker failed",
            serde_json::json!({"anchored": 1, "lexical": 1}),
        ),
        (
            "src/lib.rs",
            serde_json::json!({"path_lookup": 1, "lexical": 1}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let response = response_value(handle_semantic_search(
            &request(&format!("seam-focus-{index}"), query),
            &ctx,
        ));
        assert_eq!(response["success"], true, "{query}: {response:?}");
        assert_eq!(
            response["structuredContent"]["plan"]["callback_counts"], expected,
            "{query}"
        );
    }
}

#[test]
fn public_readiness_disclosure_executes_once_without_entering_lanes_run() {
    let (_project, ctx) = all_ready_context("http://127.0.0.1:9".to_string());
    ctx.symbol_cache()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reset();

    let response = response_value(handle_semantic_search(
        &request("seam-readiness-disclosure", "parse_header"),
        &ctx,
    ));
    assert_eq!(response["success"], true, "{response:?}");
    let plan = &response["structuredContent"]["plan"];
    assert_eq!(
        plan["lanes_run"],
        serde_json::json!(["lexical", "variants"])
    );
    assert_eq!(
        plan["executed_callbacks"],
        serde_json::json!(["lexical", "variants", "readiness_disclosure"])
    );
    assert_eq!(
        plan["callback_counts"],
        serde_json::json!({
            "lexical": 1,
            "variants": 1,
            "readiness_disclosure": 1
        })
    );
}
