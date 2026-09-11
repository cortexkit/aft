use aft::commands::semantic_search::extensions::{QueryFacts, RawQuery, Span};
use aft::commands::semantic_search::handle_semantic_search;
use aft::commands::semantic_search::plan_table::SearchShape;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_b2::router;
use aft::search_index::SearchIndex;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct ExpectedSpan {
    start: usize,
    end: usize,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RouterCase {
    query: String,
    shape: String,
    embedded_span: Option<ExpectedSpan>,
    exact_input: String,
    exact_input_tokens: usize,
    has_path_token: bool,
    has_timestamp_or_pid: bool,
}

fn cases() -> Vec<RouterCase> {
    serde_json::from_str(include_str!("fixtures/search_b2/router/cases.json"))
        .expect("parse router cases")
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
    serde_json::to_value(response).expect("serialize response")
}

fn ready_context() -> (tempfile::TempDir, AppContext) {
    let project = tempfile::tempdir().expect("create project");
    std::fs::write(
        project.path().join("lib.rs"),
        "export const foo_bar = 'ab';\npub fn parse_header() {}\npub const MESSAGE: &str = \"exceeds the cap\";\n",
    )
    .expect("write search source");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().to_path_buf()),
            ..Config::default()
        },
    );
    let index = SearchIndex::build(project.path());
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
    (project, ctx)
}

#[test]
fn router_golden_pins_precedence_delimiters_and_exact_input() {
    for case in cases() {
        let raw_query = RawQuery::new(&case.query);
        let (shape, facts) = router::classify(&raw_query);
        let expected_shape = SearchShape::from_str(&case.shape).expect("known shape label");
        assert_eq!(shape, expected_shape, "query {:?}", case.query);
        assert_eq!(
            router::exact_input(&raw_query, shape, &facts),
            case.exact_input,
            "query {:?}",
            case.query
        );

        let QueryFacts {
            embedded_span,
            exact_input_tokens,
            has_path_token,
            has_timestamp_or_pid,
        } = facts;
        assert_eq!(
            exact_input_tokens, case.exact_input_tokens,
            "query {:?}",
            case.query
        );
        assert_eq!(
            has_path_token, case.has_path_token,
            "query {:?}",
            case.query
        );
        assert_eq!(
            has_timestamp_or_pid, case.has_timestamp_or_pid,
            "query {:?}",
            case.query
        );
        match (embedded_span, case.embedded_span) {
            (None, None) => {}
            (Some(actual), Some(expected)) => {
                assert_eq!(
                    actual,
                    Span {
                        start: expected.start,
                        end: expected.end,
                    },
                    "query {:?}",
                    case.query
                );
                assert_eq!(actual.extract(&case.query), Some(expected.text.as_str()));
            }
            (actual, expected) => panic!(
                "embedded span mismatch for {:?}: actual={actual:?}, expected={expected:?}",
                case.query
            ),
        }
    }
}

#[test]
fn public_router_goldens_flow_through_handle_semantic_search() {
    let (_project, ctx) = ready_context();
    for (index, case) in cases().into_iter().enumerate() {
        let response = response_value(handle_semantic_search(
            &request(&format!("router-public-{index}"), &case.query),
            &ctx,
        ));
        assert_eq!(
            response["success"], true,
            "query {:?}: {response:?}",
            case.query
        );
        let plan = &response["structuredContent"]["plan"];
        let public_shape = if case.shape == "nl" {
            "natural_language"
        } else {
            case.shape.as_str()
        };
        assert_eq!(
            plan["shape"], public_shape,
            "query {:?}: {plan:?}",
            case.query
        );
        assert_eq!(
            plan["query_facts"]["exact_input_tokens"], case.exact_input_tokens,
            "query {:?}: {plan:?}",
            case.query
        );
    }
}

#[test]
fn empty_trimmed_public_input_is_invalid_request() {
    let (_project, ctx) = ready_context();
    let response = response_value(handle_semantic_search(
        &request("router-empty", " \t\n "),
        &ctx,
    ));
    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "invalid_request");
}

#[test]
fn whole_query_tiny_literals_use_fallback_exact_without_embedding() {
    let (_project, ctx) = ready_context();
    for (case_index, (query, exact_input)) in
        [("\"ab\"", "ab"), ("'ab'", "ab"), ("\"a b c\"", "a b c")]
            .into_iter()
            .enumerate()
    {
        for pass in ["cold", "warm"] {
            let response = response_value(handle_semantic_search(
                &request(&format!("router-literal-{case_index}-{pass}"), query),
                &ctx,
            ));
            assert_eq!(response["success"], true, "query {query}: {response:?}");
            let plan = &response["structuredContent"]["plan"];
            assert_eq!(plan["shape"], "code_literal");
            assert_eq!(plan["exact_input"], exact_input);
            assert_eq!(plan["exact_mode"], "fallback");
            assert_eq!(plan["lanes_run"], serde_json::json!(["exact", "lexical"]));
            assert_eq!(
                response["structuredContent"]["search"],
                serde_json::json!({
                    "embedding_calls": 0,
                    "embedding_cache_hits": 0,
                    "live_embed_calls": 0
                })
            );
        }
    }
}

#[test]
fn quoted_natural_language_executes_exact_against_the_embedded_span() {
    let reachability: Value = serde_json::from_str(include_str!(
        "fixtures/search_b2/router/embedded_exact_reachability.json"
    ))
    .expect("parse exact reachability oracle");
    let observed = reachability["observed_readiness"]
        .as_str()
        .expect("observed readiness cell");
    assert_eq!(reachability["oracle"][observed], "reachable_via:exact");

    let (_project, ctx) = ready_context();
    let response = response_value(handle_semantic_search(
        &request(
            "router-embedded-exact",
            "why does it print \"exceeds the cap\" here",
        ),
        &ctx,
    ));
    assert_eq!(response["success"], true, "{response:?}");
    assert_eq!(
        response["structuredContent"]["plan"]["exact_input"],
        "exceeds the cap"
    );
    assert!(response["result_count"]
        .as_u64()
        .is_some_and(|count| count > 0));
    assert_eq!(response["results"][0]["source"], "exact");
}

#[test]
fn regex_route_keeps_its_existing_owner_and_reports_zero_embedding_counts() {
    let (_project, ctx) = ready_context();
    for (index, query) in ["^export", "foo.*"].into_iter().enumerate() {
        let response = response_value(handle_semantic_search(
            &request(&format!("router-regex-{index}"), query),
            &ctx,
        ));
        assert_eq!(response["success"], true, "{query}: {response:?}");
        assert_eq!(response["query_kind"], "Regex");
        assert_eq!(response["interpreted_as"], "regex");
        assert_eq!(response["structuredContent"]["plan"]["shape"], "regex");
        assert_eq!(
            response["structuredContent"]["plan"]["lanes_run"],
            serde_json::json!(["fallback_walk"])
        );
        assert_eq!(
            response["structuredContent"]["search"],
            serde_json::json!({
                "embedding_calls": 0,
                "embedding_cache_hits": 0,
                "live_embed_calls": 0
            })
        );
    }
}

#[test]
fn filename_detection_delegates_to_the_existing_exemption_authority() {
    let router_source = include_str!("../src/search_b2/router.rs");
    let authority_source = include_str!("../src/query_shape.rs");
    assert!(router_source.contains("query_shape::pre_tier_exempt(token)"));
    assert!(authority_source.contains("static FILENAME_EXEMPTION_RE"));
}
