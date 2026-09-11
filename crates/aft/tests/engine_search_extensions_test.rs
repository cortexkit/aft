use std::sync::atomic::{AtomicUsize, Ordering};

use aft::commands::semantic_search::extensions::{
    DefaultSearchExtensions, RawQuery, Readiness, Root, SearchExtensions,
};
use aft::commands::semantic_search::{
    LaneExecution, LaneInput, LaneRegistry, SearchLane, SearchLaneKind, SearchShape,
};
use aft::search_index::SearchIndex;

#[derive(Default)]
struct CountingLane {
    calls: AtomicUsize,
}

impl SearchLane for CountingLane {
    fn kind(&self) -> SearchLaneKind {
        SearchLaneKind::Lexical
    }

    fn execute(&self, _input: &LaneInput<'_>) -> LaneExecution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        LaneExecution {
            kind: self.kind(),
            candidates: Vec::new(),
        }
    }
}

#[test]
fn raw_query_preserves_quotes_and_default_classification_covers_all_seven_shapes() {
    let extensions = DefaultSearchExtensions;
    let cases = [
        ("handleRequest", SearchShape::Identifier),
        ("\"literal phrase\"", SearchShape::CodeLiteral),
        ("rate limiting", SearchShape::Short),
        (
            "where is request authentication handled",
            SearchShape::NaturalLanguage,
        ),
        (
            "2026-09-10T08:12:00Z ERROR worker request failed",
            SearchShape::LogExcerpt,
        ),
        ("src/commands/search.rs", SearchShape::Path),
        ("^export\\s+function", SearchShape::Regex),
    ];

    for (query, expected) in cases {
        let raw_query = RawQuery::new(query);
        let (shape, _facts) = extensions.classify(&raw_query);
        assert_eq!(raw_query.original_query(), query);
        assert_eq!(shape, expected, "query: {query}");
    }
}

#[test]
fn default_plan_omits_semantic_for_code_literals_and_logs() {
    let extensions = DefaultSearchExtensions;
    let readiness = Readiness::new(true, true, true);
    for query in ["\"literal phrase\"", "2026-09-10 ERROR worker failed"] {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = extensions.classify(&raw_query);
        let plan = extensions.plan(&shape, &facts, &readiness);
        assert!(!plan.contains(SearchLaneKind::Semantic), "query: {query}");
    }
}

#[test]
fn registered_lane_execution_uses_the_extension_callback() {
    let root = tempfile::tempdir().unwrap();
    let index = SearchIndex::new();
    let lane = std::sync::Arc::new(CountingLane::default());
    let mut registry = LaneRegistry::new();
    registry.register(lane.clone());

    let input = LaneInput {
        query: "needle",
        shape: SearchShape::Identifier,
        root: root.path(),
        include_tests: false,
        index: &index,
    };
    let extensions = DefaultSearchExtensions;
    let registered = registry.get(SearchLaneKind::Lexical).unwrap();
    let output = extensions.execute_lane(registered.as_ref(), &input);

    assert_eq!(output.kind, SearchLaneKind::Lexical);
    assert_eq!(lane.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn readiness_is_sampled_from_the_root_snapshot() {
    let extensions = DefaultSearchExtensions;
    let expected = Readiness::new(false, true, false);
    let root = Root::new(".", expected.clone());
    assert_eq!(extensions.sample_readiness(&root), expected);
}

#[test]
fn classify_tuple_passes_all_four_query_facts_to_plan_without_rederivation() {
    let extensions = DefaultSearchExtensions;
    let cases = [
        ("where do we cap the room name", None, 5),
        (
            "why does it print \"exceeds the cap\" here",
            Some((19, 34)),
            3,
        ),
        ("why does it print \"ab\" here", Some((19, 21)), 0),
    ];

    for (query, expected_span, expected_tokens) in cases {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = extensions.classify(&raw_query);
        assert_eq!(shape, SearchShape::NaturalLanguage);
        let span = facts.embedded_span.map(|span| (span.start, span.end));
        assert_eq!(span, expected_span, "query: {query}");
        assert_eq!(facts.exact_input_tokens, expected_tokens, "query: {query}");
        assert!(!facts.has_path_token, "query: {query}");
        assert!(!facts.has_timestamp_or_pid, "query: {query}");
        assert_eq!(
            extensions
                .plan(&shape, &facts, &Readiness::new(true, true, true))
                .query_facts,
            facts,
            "query: {query}"
        );
    }
}
