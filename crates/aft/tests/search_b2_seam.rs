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
        [
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
        ]
    );
    assert_eq!(
        plan.executed_callbacks,
        [
            SearchLaneKind::Exact,
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
        "^export",
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
            serde_json::json!({"exact": 1, "anchored": 1, "lexical": 1}),
        ),
        (
            "src/lib.rs",
            serde_json::json!({"exact": 1, "path_lookup": 1, "lexical": 1}),
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
        serde_json::json!(["exact", "lexical", "variants"])
    );
    assert_eq!(
        plan["executed_callbacks"],
        serde_json::json!(["exact", "lexical", "variants", "readiness_disclosure"])
    );
    assert_eq!(
        plan["callback_counts"],
        serde_json::json!({
            "exact": 1,
            "lexical": 1,
            "variants": 1,
            "readiness_disclosure": 1
        })
    );
}

fn path_fact_context(base_url: String) -> (tempfile::TempDir, AppContext) {
    let project = tempfile::tempdir().expect("create path-fact project");
    let source_dir = project.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create path-fact source directory");
    let named_file = source_dir.join("subc_format.rs");
    let other_file = source_dir.join("context.rs");
    let named_source = concat!(
        "pub fn callgraph_op() {}\n",
        "pub struct Tier2PhaseTimings;\n",
        "// alpha beta gamma live in the formatter implementation\n"
    );
    let other_source = concat!(
        "pub fn callgraph_op_helper() {}\n",
        "pub fn live() {}\n",
        "// callgraph_op subc_format.rs appears in routing documentation\n",
        "// alpha beta gamma live in another implementation\n"
    );
    std::fs::write(&named_file, named_source).expect("write named path fixture");
    std::fs::write(&other_file, other_source).expect("write competing path fixture");

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
    let files = [named_file.clone(), other_file.clone()];
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
    let semantic_index =
        SemanticIndex::build(project.path(), &files, &mut embed, 16).expect("build path semantic");
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    *ctx.semantic_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(semantic_index);
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SearchIndex::build(project.path()));
    for (path, source) in [(&named_file, named_source), (&other_file, other_source)] {
        let metadata = std::fs::metadata(path).expect("path-fact source metadata");
        ctx.symbol_cache()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                path.clone(),
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                metadata.len(),
                blake3::hash(source.as_bytes()),
                Vec::new(),
            );
    }
    (project, ctx)
}

fn result_files(response: &Value) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .filter_map(|result| {
            let normalized = result["file"]
                .as_str()
                .expect("result file")
                .replace('\\', "/");
            let ranked_file = normalized
                .rfind("/src/")
                .map_or(normalized.as_str(), |offset| &normalized[offset + 1..])
                .to_string();
            seen.insert(ranked_file.clone()).then_some(ranked_file)
        })
        .collect()
}

#[test]
fn mixed_filename_fact_restores_the_pinned_path_result_to_rank_one() {
    let (base_url, server) = start_embedding_server();
    let (_project, ctx) = path_fact_context(base_url);
    let response = response_value(handle_semantic_search(
        &request("r1a-pinned-path", "callgraph_op subc_format.rs"),
        &ctx,
    ));
    server.join().expect("embedding server");

    assert_eq!(response["structuredContent"]["plan"]["shape"], "short");
    assert_eq!(
        response["structuredContent"]["plan"]["lanes_run"],
        serde_json::json!([
            "symbol",
            "exact",
            "lexical",
            "variants",
            "semantic",
            "path_lookup"
        ])
    );
    assert!(
        result_files(&response)[0].ends_with("src/subc_format.rs"),
        "ranked files: {:?}; response: {response:?}",
        result_files(&response)
    );
}

#[test]
fn unresolved_filename_fact_does_not_change_ranked_files() {
    let (base_url, server) = start_embedding_server();
    let (_project, ctx) = path_fact_context(base_url);
    let plain = response_value(handle_semantic_search(
        &request("r1a-unresolved-control", "alpha beta"),
        &ctx,
    ));
    let unresolved = response_value(handle_semantic_search(
        &request("r1a-unresolved-path", "alpha beta nosuchfile.rs"),
        &ctx,
    ));
    server.join().expect("embedding server");

    assert_eq!(result_files(&unresolved), result_files(&plain));
}

#[test]
fn natural_language_filename_fact_keeps_semantic_and_prioritizes_the_named_file() {
    let (base_url, server) = start_embedding_server();
    let (_project, ctx) = path_fact_context(base_url);
    let response = response_value(handle_semantic_search(
        &request(
            "r1a-natural-language-path",
            "where does alpha beta gamma live in subc_format.rs today please",
        ),
        &ctx,
    ));
    server.join().expect("embedding server");

    let plan = &response["structuredContent"]["plan"];
    assert_eq!(plan["shape"], "natural_language");
    assert!(plan["lanes_run"]
        .as_array()
        .expect("lanes")
        .contains(&serde_json::json!("semantic")));
    assert!(
        result_files(&response)[0].ends_with("src/subc_format.rs"),
        "ranked files: {:?}; response: {response:?}",
        result_files(&response)
    );
}

#[test]
fn natural_language_without_identifier_token_does_not_select_symbol() {
    use aft::commands::semantic_search::extensions::{RawQuery, Readiness};
    use aft::commands::semantic_search::plan_table::SearchLaneKind;
    use aft::search_b2::install_defaults;

    let extensions = install_defaults();
    let query = RawQuery::new("where does alpha beta gamma live in the service today");
    let (shape, facts) = extensions.classify(&query);
    let plan = extensions.plan(&shape, &facts, &Readiness::new(true, true, true));
    assert_eq!(shape.as_str(), "nl");
    assert!(!facts.has_identifier_token);
    assert!(!plan.selected_lanes.contains(&SearchLaneKind::Symbol));
}

#[test]
fn unresolved_natural_language_identifier_keeps_baseline_ordering() {
    let plain_query = "where does missingtargettype live with alpha beta gamma today";
    let identifier_query = "where does MissingTargetType live with alpha beta gamma today";

    let (plain_url, plain_server) = start_embedding_server();
    let (_plain_project, plain_ctx) = path_fact_context(plain_url);
    let plain = response_value(handle_semantic_search(
        &request("r1b-plain-control", plain_query),
        &plain_ctx,
    ));
    plain_server.join().expect("plain embedding server");

    let (identifier_url, identifier_server) = start_embedding_server();
    let (_identifier_project, identifier_ctx) = path_fact_context(identifier_url);
    let identifier = response_value(handle_semantic_search(
        &request("r1b-unresolved-identifier", identifier_query),
        &identifier_ctx,
    ));
    identifier_server
        .join()
        .expect("identifier embedding server");

    assert_eq!(
        plain["structuredContent"]["plan"]["shape"],
        "natural_language"
    );
    assert_eq!(
        identifier["structuredContent"]["plan"]["shape"],
        "natural_language"
    );
    assert!(!plain["structuredContent"]["plan"]["lanes_run"]
        .as_array()
        .expect("plain lanes")
        .contains(&serde_json::json!("symbol")));
    assert!(identifier["structuredContent"]["plan"]["lanes_run"]
        .as_array()
        .expect("identifier lanes")
        .contains(&serde_json::json!("symbol")));
    assert_eq!(result_files(&identifier), result_files(&plain));
}

#[test]
fn natural_language_identifier_definition_is_ranked_first() {
    let (base_url, server) = start_embedding_server();
    let (_project, ctx) = path_fact_context(base_url);
    let response = response_value(handle_semantic_search(
        &request(
            "r1b-definition-first",
            "perf tier2 phases freshness snapshot scan db rollup log emit Tier2PhaseTimings",
        ),
        &ctx,
    ));
    server.join().expect("embedding server");

    let plan = &response["structuredContent"]["plan"];
    assert_eq!(plan["shape"], "natural_language");
    assert!(plan["lanes_run"]
        .as_array()
        .expect("lanes")
        .contains(&serde_json::json!("symbol")));
    assert!(
        result_files(&response)[0].ends_with("src/subc_format.rs"),
        "ranked files: {:?}; response: {response:?}",
        result_files(&response)
    );
    assert_eq!(response["results"][0]["exact"], true);
}

#[test]
fn plain_natural_language_verbatim_phrase_runs_exact() {
    let (base_url, server) = start_embedding_server();
    let (_project, ctx) = path_fact_context(base_url);
    let response = response_value(handle_semantic_search(
        &request(
            "r2a-plain-natural-language-exact",
            "alpha beta gamma live in the formatter implementation",
        ),
        &ctx,
    ));
    server.join().expect("embedding server");

    let plan = &response["structuredContent"]["plan"];
    assert_eq!(plan["shape"], "natural_language");
    assert!(plan["lanes_run"]
        .as_array()
        .expect("lanes")
        .contains(&serde_json::json!("exact")));
    assert!(result_files(&response)[0].ends_with("src/subc_format.rs"));
    assert_eq!(response["results"][0]["exact"], true);
}

#[test]
fn named_file_precedence_preserves_outside_exact_evidence() {
    let (base_url, server) = start_embedding_server();
    let (project, ctx) = path_fact_context(base_url);
    for file in ["subc_format.rs", "context.rs"] {
        use std::io::Write as _;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(project.path().join("src").join(file))
                .expect("open exact precedence fixture"),
            "// shared_exact subc_format.rs"
        )
        .expect("append exact precedence fixture");
    }
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(SearchIndex::build(project.path()));
    let response = response_value(handle_semantic_search(
        &request("r1a-stable-path-precedence", "shared_exact subc_format.rs"),
        &ctx,
    ));
    server.join().expect("embedding server");

    let results = response["results"].as_array().expect("results");
    let named_position = results
        .iter()
        .position(|result| {
            result["file"]
                .as_str()
                .is_some_and(|path| path.ends_with("src/subc_format.rs"))
        })
        .expect("named file result");
    let outside_position = results
        .iter()
        .position(|result| {
            result["file"]
                .as_str()
                .is_some_and(|path| path.ends_with("src/context.rs"))
        })
        .expect("outside exact result");

    assert_eq!(named_position, 0, "response: {response:?}");
    assert!(named_position < outside_position, "response: {response:?}");
    assert_eq!(results[named_position]["exact"], true);
    assert_eq!(results[outside_position]["exact"], true);
}
