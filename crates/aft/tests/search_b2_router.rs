use aft::commands::semantic_search::extensions::{QueryFacts, RawQuery, Span};
use aft::commands::semantic_search::plan_table::SearchShape;
use aft::search_b2::router;
use serde::Deserialize;

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
    has_identifier_token: bool,
}

fn cases() -> Vec<RouterCase> {
    serde_json::from_str(include_str!("fixtures/search_b2/router/cases.json"))
        .expect("parse router cases")
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
            has_identifier_token,
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
        assert_eq!(
            has_identifier_token, case.has_identifier_token,
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
fn filename_detection_delegates_to_the_existing_exemption_authority() {
    let router_source = include_str!("../src/search_b2/router.rs");
    let authority_source = include_str!("../src/query_shape.rs");
    assert!(router_source.contains("query_shape::pre_tier_exempt(token)"));
    assert!(authority_source.contains("static FILENAME_EXEMPTION_RE"));
}

#[test]
fn natural_language_identifier_facts_preserve_dotted_snake_kebab_and_camel_tokens() {
    let cases = [
        "manager.ingest_event wire operation handler in core module",
        "ingest_canonical event into store task lifecycle",
        "cortexkit-cow isolation backend diff usage",
        "how is transform_mode resolved per session from project config",
        "where is Tier2PhaseTimings assembled in the inspect manager today",
    ];

    for query in cases {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = router::classify(&raw_query);
        assert_eq!(shape, SearchShape::NaturalLanguage, "{query}");
        assert!(facts.has_identifier_token, "{query}");
    }
}
