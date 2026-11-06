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
            SearchLaneKind::Semantic,
        ]
    );
    assert_eq!(
        plan.executed_callbacks,
        [
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
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
