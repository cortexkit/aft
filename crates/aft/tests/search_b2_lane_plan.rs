use std::collections::{BTreeMap, BTreeSet};

use aft::commands::semantic_search::extensions::{
    ExactMode, QueryFacts, RawQuery, Readiness, RetainedReadinessSnapshots, Span,
};
use aft::commands::semantic_search::plan_table::{SearchLaneKind, SearchShape};
use aft::search_b2::lane_plan::legal_lanes;
use aft::search_b2::readiness::selected_lane_disclosures;
use aft::search_b2::{install_defaults, router};
use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ExpectedFacts {
    embedded_span: Option<SpanExpectation>,
    exact_input_tokens: usize,
    has_path_token: bool,
    has_timestamp_or_pid: bool,
    has_identifier_token: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct SpanExpectation {
    start: usize,
    end: usize,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct CounterExpectation {
    embedding_calls: usize,
    embedding_cache_hits: usize,
    live_embed_calls: usize,
}

#[derive(Debug, Deserialize)]
struct MatrixRow {
    query_class: String,
    readiness: String,
    query: String,
    shape: String,
    query_facts: ExpectedFacts,
    exact_input: Option<String>,
    exact_mode: String,
    lanes_run: Vec<String>,
    executed_callbacks: Vec<String>,
    embedding_counters_cold: CounterExpectation,
    footer_lines: Vec<String>,
}

fn rows() -> Vec<MatrixRow> {
    serde_json::from_str(include_str!("fixtures/search_b2/lane_plan/matrix.json"))
        .expect("parse lane-plan matrix")
}

fn lane(label: &str) -> SearchLaneKind {
    SearchLaneKind::from_str(label).unwrap_or_else(|| panic!("unknown lane {label}"))
}

fn readiness(bits: &str) -> Readiness<'static> {
    let bits = bits.as_bytes();
    assert_eq!(bits.len(), 3);
    let semantic = bits[0] == b'1';
    let trigram = bits[1] == b'1';
    let symbol = bits[2] == b'1';
    let mut reasons = Vec::new();
    if !trigram {
        reasons.push("trigram:disabled".to_string());
    }
    if !symbol {
        reasons.push("symbol:disabled".to_string());
    }
    if !semantic {
        reasons.push("semantic:disabled".to_string());
    }
    Readiness::observed(
        symbol,
        trigram,
        semantic,
        reasons,
        RetainedReadinessSnapshots::default(),
        false,
    )
}

#[test]
fn independent_eighty_row_matrix_matches_installed_runtime_hooks() {
    let rows = rows();
    assert_eq!(rows.len(), 80);
    let cells = rows
        .iter()
        .map(|row| (row.query_class.as_str(), row.readiness.as_str()))
        .collect::<BTreeSet<_>>();
    assert_eq!(cells.len(), 80);
    let by_class = rows.iter().fold(BTreeMap::new(), |mut counts, row| {
        *counts.entry(row.query_class.as_str()).or_insert(0usize) += 1;
        counts
    });
    assert_eq!(by_class.len(), 10);
    assert!(by_class.values().all(|count| *count == 8));

    for row in rows {
        let raw_query = RawQuery::new(&row.query);
        let (shape, facts) = install_defaults().classify(&raw_query);
        assert_eq!(
            shape,
            SearchShape::from_str(&row.shape).expect("known shape"),
            "{} {}",
            row.query_class,
            row.readiness
        );
        assert_eq!(
            facts,
            QueryFacts {
                embedded_span: row.query_facts.embedded_span.as_ref().map(|span| Span {
                    start: span.start,
                    end: span.end,
                }),
                exact_input_tokens: row.query_facts.exact_input_tokens,
                has_path_token: row.query_facts.has_path_token,
                has_timestamp_or_pid: row.query_facts.has_timestamp_or_pid,
                has_identifier_token: row.query_facts.has_identifier_token,
            },
            "{} {}",
            row.query_class,
            row.readiness
        );

        let readiness = readiness(&row.readiness);
        let mut plan = install_defaults().plan(&shape, &facts, &readiness);
        if plan.contains(SearchLaneKind::Exact) {
            plan.exact_input = Some(router::exact_input(&raw_query, shape, &facts));
        }
        assert_eq!(
            plan.exact_input, row.exact_input,
            "{} {} exact input",
            row.query_class, row.readiness
        );
        assert_eq!(
            serde_json::to_value(plan.exact_mode).unwrap(),
            row.exact_mode,
            "{} {} exact mode",
            row.query_class,
            row.readiness
        );
        assert_eq!(
            plan.selected_lanes,
            row.lanes_run
                .iter()
                .map(|label| lane(label))
                .collect::<Vec<_>>(),
            "{} {} lanes",
            row.query_class,
            row.readiness
        );
        assert_eq!(
            plan.executed_callbacks,
            row.executed_callbacks
                .iter()
                .map(|label| lane(label))
                .collect::<Vec<_>>(),
            "{} {} callbacks",
            row.query_class,
            row.readiness
        );

        let legal = legal_lanes(shape, &facts);
        assert_eq!(
            selected_lane_disclosures(&legal, &readiness, &[], None),
            row.footer_lines,
            "{} {} footer",
            row.query_class,
            row.readiness
        );
        let semantic_selected = plan.contains(SearchLaneKind::Semantic);
        assert_eq!(
            CounterExpectation {
                embedding_calls: usize::from(semantic_selected),
                embedding_cache_hits: 0,
                live_embed_calls: 0,
            },
            row.embedding_counters_cold,
            "{} {} counters",
            row.query_class,
            row.readiness
        );
    }
}

#[test]
fn deterministic_degradation_never_adds_semantic_to_identifiers_or_literals() {
    for query in ["parse_header", "\"exceeds the cap\"", "\"ab\""] {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = install_defaults().classify(&raw_query);
        for bits in ["111", "110", "101", "100", "011", "010", "001", "000"] {
            let plan = install_defaults().plan(&shape, &facts, &readiness(bits));
            assert!(!plan.contains(SearchLaneKind::Semantic), "{query} {bits}");
            assert_ne!(plan.selected_lanes, Vec::<SearchLaneKind>::new());
        }
    }
}

#[test]
fn exact_mode_uses_exact_input_tokens_not_surrounding_prose() {
    let extensions = install_defaults();
    let ready = readiness("111");
    for (query, expected) in [
        ("why does it print \"ab\" here", ExactMode::Fallback),
        (
            "why does it print \"exceeds the cap\" here",
            ExactMode::Ready,
        ),
    ] {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = extensions.classify(&raw_query);
        assert_eq!(shape, SearchShape::NaturalLanguage);
        assert_eq!(extensions.plan(&shape, &facts, &ready).exact_mode, expected);
    }
}
