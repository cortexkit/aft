use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use aft::commands::semantic_search::{blocks, evidence_descriptor, paging, plan_table, scoring};

use blocks::{BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate};
use evidence_descriptor::EvidenceDescriptor;
use paging::{
    build_reference_list as build_l, parse_public_page_request, serve_public_page, StopState,
    ValidatedPageRequest,
};
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use scoring::ScoringPolicy;

#[derive(Debug, Deserialize)]
struct Fixture {
    schema: u32,
    sparse: SparseFixture,
    start_tier: StartTierFixture,
    ordinary: OrdinaryFixture,
    exhausted_out_of_range: OutOfRangeFixture,
    depth_cap_out_of_range: OutOfRangeFixture,
    coincidences: CoincidenceFixture,
    above_cap: AboveCapFixture,
}

#[derive(Debug, Deserialize)]
struct SparseFixture {
    offset: usize,
    top_k: usize,
    expected_depth: usize,
    expected_tier: usize,
}

#[derive(Debug, Deserialize)]
struct StartTierFixture {
    tier_zero_count: usize,
    late_offset: usize,
    page_size: usize,
}

#[derive(Debug, Deserialize)]
struct OrdinaryFixture {
    shown: usize,
    total: usize,
    depth: usize,
}

#[derive(Debug, Deserialize)]
struct OutOfRangeFixture {
    offset: usize,
    total: usize,
}

#[derive(Debug, Deserialize)]
struct CoincidenceFixture {
    c1_offset: usize,
    c1_top_k: usize,
    c2_offset: usize,
    c2_top_k: usize,
    c3_offset: usize,
    c3_top_k: usize,
    c5_top_k: usize,
}

#[derive(Debug, Deserialize)]
struct AboveCapFixture {
    offset: usize,
    top_k: usize,
    depth: usize,
    tier: usize,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/paging/cases.json"
    ))
    .expect("paging fixture must be valid JSON")
}

fn policy() -> ScoringPolicy {
    ScoringPolicy::from_plan_table(&PlanTable::running_table(), SearchShape::NaturalLanguage)
        .expect("running natural-language scoring policy")
}

fn key(label: &str, include_tests: bool) -> CanonicalListKey {
    CanonicalListKey {
        project_root: PathBuf::from(format!("/virtual/{label}")),
        snapshot_generation: "paging-generation-1".to_string(),
        normalized_query: label.to_string(),
        include_tests,
    }
}

fn candidate(path: impl Into<PathBuf>, score: f32, is_test: bool) -> LaneCandidate {
    LaneCandidate::non_exact(
        path,
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        score,
        is_test,
    )
}

fn lane(candidates: Vec<LaneCandidate>) -> CanonicalLane {
    CanonicalLane::new(SearchLaneKind::Semantic, candidates).expect("canonical semantic lane")
}

fn builder(label: &str, candidates: Vec<LaneCandidate>, include_tests: bool) -> BlockBuilder {
    BlockBuilder::new(key(label, include_tests), policy(), vec![lane(candidates)])
        .expect("paging block builder")
}

fn request(offset: usize, top_k: usize) -> ValidatedPageRequest {
    parse_public_page_request(&json!({ "offset": offset, "topK": top_k }))
        .expect("valid public page request")
}

fn paths(page: &paging::SearchPage) -> Vec<PathBuf> {
    page.reply
        .page
        .iter()
        .map(|entry| entry.result.path.clone())
        .collect()
}

fn numbered_candidates(count: usize) -> Vec<LaneCandidate> {
    (0..count)
        .map(|position| {
            candidate(
                format!("result-{position:04}.rs"),
                1.0 - position as f32 / 10_000.0,
                false,
            )
        })
        .collect()
}

#[test]
fn public_domain_is_strict_and_interval_arithmetic_is_post_validation() {
    let invalid = [
        (json!({"offset": -1, "topK": 10}), "offset", "MAX_OFFSET"),
        (json!({"offset": 1.5, "topK": 10}), "offset", "MAX_OFFSET"),
        (
            json!({"offset": 100001, "topK": 10}),
            "offset",
            "MAX_OFFSET",
        ),
        (json!({"offset": 0, "topK": 300}), "topK", "MAX_TOP_K"),
        (json!({"offset": 0, "topK": 101}), "topK", "MAX_TOP_K"),
        (json!({"offset": 0, "topK": 0}), "topK", "MAX_TOP_K"),
        (json!({"offset": 0, "topK": -1}), "topK", "MAX_TOP_K"),
        (json!({"offset": 0, "topK": 1.5}), "topK", "MAX_TOP_K"),
        (
            json!({"offset": u64::MAX, "topK": 1}),
            "offset",
            "MAX_OFFSET",
        ),
    ];
    for (params, field, bound) in invalid {
        let error = parse_public_page_request(&params).expect_err("request must be rejected");
        assert_eq!(error.code(), "invalid_request");
        assert_eq!(error.field(), field);
        assert!(error.to_string().contains(bound), "{error}");
    }

    for top_k in [1, 100] {
        let parsed = request(100_000, top_k);
        assert_eq!(parsed.offset(), 100_000);
        assert_eq!(parsed.top_k(), top_k);
        assert_eq!(
            parsed.interval_end(),
            100_000u64.saturating_add(top_k as u64)
        );
    }

    let source = include_str!("../src/commands/semantic_search/paging.rs");
    assert!(source.contains("(self.offset as u64).saturating_add(self.top_k as u64)"));
    assert!(!source.contains(".clamp(1, MAX_PUBLIC_TOP_K)"));
}

#[test]
fn top_k_out_of_range_is_never_clamped_into_a_page() {
    let search = builder("strict-top-k", numbered_candidates(400), false);
    for top_k in [0, 101, 300] {
        let error = parse_public_page_request(&json!({"offset": 0, "topK": top_k}))
            .expect_err("out-of-range topK must not produce a page");
        assert_eq!(error.code(), "invalid_request");
        assert!(error.to_string().contains("MAX_TOP_K"));
    }
    for top_k in [1, 100] {
        let page = serve_public_page(&search, request(0, top_k)).expect("bounded page");
        assert_eq!(page.shown(), top_k);
    }
}

#[test]
fn build_l_is_harness_only_and_public_top_k_300_is_rejected() {
    let paging_source = include_str!("../src/commands/semantic_search/paging.rs");
    let backend_surface = include_str!("../src/commands/semantic_search/mod.rs");
    let agent_surface = include_str!("../src/subc_translate.rs");
    assert!(paging_source.contains("pub(crate) fn build_l"));
    assert!(!backend_surface.contains("build_l"));
    assert!(!agent_surface.contains("build_l"));

    let error = parse_public_page_request(&json!({"topK": 300}))
        .expect_err("reference retrieval must not widen the public domain");
    assert_eq!(error.code(), "invalid_request");
    assert!(error.to_string().contains("MAX_TOP_K"));
}

#[test]
fn sparse_deduplicated_tier_escalates_and_returns_results_21_through_30() {
    let fixture = fixture();
    assert_eq!(fixture.schema, 1);
    let mut candidates = numbered_candidates(20);
    candidates.extend((20..200).map(|position| {
        candidate(
            format!("tests/filtered-{position:04}.rs"),
            0.8 - position as f32 / 10_000.0,
            true,
        )
    }));
    candidates.extend((20..40).map(|position| {
        candidate(
            format!("result-{position:04}.rs"),
            0.6 - position as f32 / 10_000.0,
            false,
        )
    }));
    let duplicate_lane = CanonicalLane::new(SearchLaneKind::Lexical, numbered_candidates(20))
        .expect("canonical duplicate lane");
    let search = BlockBuilder::new(
        key("sparse-tier", false),
        policy(),
        vec![lane(candidates), duplicate_lane],
    )
    .expect("sparse deduplicated builder");
    let page = serve_public_page(
        &search,
        request(fixture.sparse.offset, fixture.sparse.top_k),
    )
    .expect("sparse page");

    assert_eq!(page.reply.retrieval_depth, fixture.sparse.expected_depth);
    assert_eq!(page.reply.depth_tier, fixture.sparse.expected_tier);
    assert_eq!(page.stop_state, StopState::S2Exhausted);
    assert_eq!(
        paths(&page),
        (20..30)
            .map(|position| PathBuf::from(format!("result-{position:04}.rs")))
            .collect::<Vec<_>>()
    );
}

#[test]
fn successive_pages_and_direct_high_offset_equal_the_reference_prefix() {
    let mut candidates = numbered_candidates(20);
    candidates.extend((20..200).map(|position| {
        candidate(
            format!("tests/hidden-{position:04}.rs"),
            0.8 - position as f32 / 10_000.0,
            true,
        )
    }));
    candidates.extend((20..45).map(|position| {
        candidate(
            format!("result-{position:04}.rs"),
            0.6 - position as f32 / 10_000.0,
            false,
        )
    }));
    let search = builder("successive-pages", candidates, false);
    let first = serve_public_page(&search, request(0, 10)).expect("first page");
    let second = serve_public_page(&search, request(10, 10)).expect("second page");
    let direct_later = serve_public_page(&search, request(20, 10)).expect("direct later page");
    assert_eq!(first.reply.retrieval_depth, 200);
    assert_eq!(second.reply.retrieval_depth, 200);
    assert_eq!(direct_later.reply.retrieval_depth, 400);
    assert!(!first.stability_void && !second.stability_void && !direct_later.stability_void);
    assert_eq!(
        first.reply.canonical_list.key.snapshot_generation,
        second.reply.canonical_list.key.snapshot_generation
    );
    assert_eq!(
        second.reply.canonical_list.key.snapshot_generation,
        direct_later.reply.canonical_list.key.snapshot_generation
    );

    let mut concatenated = first.reply.page_stability_units();
    concatenated.extend(second.reply.page_stability_units());
    concatenated.extend(direct_later.reply.page_stability_units());
    let reference = build_l(&search, 0..30).expect("reference prefix");
    assert_eq!(reference.retrieval_depth, 400);
    assert_eq!(reference.depth_tier, 1);
    assert!(reference.lanes_exhausted);
    assert_eq!(concatenated, reference.stability_units);

    let identities = concatenated
        .iter()
        .map(|unit| (&unit.ranked_tuple.file, unit.ranked_tuple.symbol_range))
        .collect::<HashSet<_>>();
    assert_eq!(
        identities.len(),
        concatenated.len(),
        "no duplicate stability identity"
    );
}

#[test]
fn starting_above_tier_zero_reconstructs_frozen_prefix_blocks() {
    let fixture = fixture();
    let mut candidates = numbered_candidates(fixture.start_tier.tier_zero_count);
    candidates.push(candidate("tier-one-high-score.rs", 100.0, false));
    let search = builder("start-tier-independence", candidates, false);

    let mut concatenated = Vec::new();
    for offset in [0, 100, fixture.start_tier.late_offset] {
        let page = serve_public_page(&search, request(offset, fixture.start_tier.page_size))
            .expect("direct independent page");
        concatenated.extend(page.reply.page_stability_units());
    }
    let reference = build_l(&search, 0..300).expect("reference list through tier one");
    assert_eq!(concatenated, reference.stability_units);
    assert_eq!(concatenated.len(), 201);
    assert!(concatenated[..200]
        .iter()
        .all(|unit| unit.ranked_tuple.file != PathBuf::from("tier-one-high-score.rs")));
    assert_eq!(
        concatenated[200].ranked_tuple.file,
        PathBuf::from("tier-one-high-score.rs")
    );
}

#[test]
fn above_cap_requests_start_at_d_max_for_exhausted_and_unexplored_lanes() {
    let fixture = fixture();
    for (count, expected_state, expected_exhausted) in [
        (3200, StopState::S2Exhausted, true),
        (3201, StopState::S3DepthCap, false),
    ] {
        let search = builder(
            &format!("above-cap-{count}"),
            numbered_candidates(count),
            false,
        );
        let page = serve_public_page(
            &search,
            request(fixture.above_cap.offset, fixture.above_cap.top_k),
        )
        .expect("valid above-cap request");
        assert!(page.reply.page.is_empty());
        assert_eq!(page.reply.retrieval_depth, fixture.above_cap.depth);
        assert_eq!(page.reply.depth_tier, fixture.above_cap.tier);
        assert_eq!(page.reply.lanes_exhausted, expected_exhausted);
        assert_eq!(page.stop_state, expected_state);
    }
}

#[test]
fn ordinary_out_of_range_coincidence_and_head_pair_states_are_exact() {
    let fixture = fixture();

    let ordinary = builder("ordinary", numbered_candidates(201), false);
    let ordinary_page =
        serve_public_page(&ordinary, request(0, fixture.ordinary.shown)).expect("ordinary page");
    assert_eq!(ordinary_page.shown(), fixture.ordinary.shown);
    assert_eq!(ordinary_page.total_at_stop(), fixture.ordinary.total);
    assert_eq!(ordinary_page.reply.retrieval_depth, fixture.ordinary.depth);
    assert_eq!(ordinary_page.stop_state, StopState::S1MoreAtDepth);

    let exhausted = builder(
        "exhausted-out-of-range",
        numbered_candidates(fixture.exhausted_out_of_range.total),
        false,
    );
    let exhausted_page = serve_public_page(
        &exhausted,
        request(fixture.exhausted_out_of_range.offset, 10),
    )
    .expect("exhausted out-of-range page");
    assert!(exhausted_page.reply.page.is_empty());
    assert_eq!(exhausted_page.stop_state, StopState::S2Exhausted);

    let mut depth_cap_candidates = numbered_candidates(fixture.depth_cap_out_of_range.total);
    depth_cap_candidates.extend(
        (400..3200).map(|position| candidate(format!("tests/hidden-{position:04}.rs"), 0.2, true)),
    );
    depth_cap_candidates.push(candidate("still-unexplored.rs", 0.1, false));
    let depth_cap = builder("depth-cap-out-of-range", depth_cap_candidates, false);
    let depth_cap_page = serve_public_page(
        &depth_cap,
        request(fixture.depth_cap_out_of_range.offset, 10),
    )
    .expect("depth-cap out-of-range page");
    assert!(depth_cap_page.reply.page.is_empty());
    assert_eq!(
        depth_cap_page.total_at_stop(),
        fixture.depth_cap_out_of_range.total
    );
    assert_eq!(depth_cap_page.stop_state, StopState::S3DepthCap);

    let c1 = builder("c1", numbered_candidates(3201), false);
    let c1_page = serve_public_page(
        &c1,
        request(
            fixture.coincidences.c1_offset,
            fixture.coincidences.c1_top_k,
        ),
    )
    .expect("C1 page");
    assert_eq!(c1_page.shown(), 10);
    assert_eq!(c1_page.total_at_stop(), 3200);
    assert_eq!(c1_page.stop_state, StopState::S3DepthCap);

    let c2 = builder("c2", numbered_candidates(210), false);
    let c2_page = serve_public_page(
        &c2,
        request(
            fixture.coincidences.c2_offset,
            fixture.coincidences.c2_top_k,
        ),
    )
    .expect("C2 page");
    assert_eq!(c2_page.shown(), 10);
    assert_eq!(c2_page.stop_state, StopState::S2Exhausted);
    assert_eq!(c2_page.reply.retrieval_depth, 400);

    let c3 = builder("c3", numbered_candidates(3200), false);
    let c3_page = serve_public_page(
        &c3,
        request(
            fixture.coincidences.c3_offset,
            fixture.coincidences.c3_top_k,
        ),
    )
    .expect("C3 page");
    assert_eq!(c3_page.shown(), 10);
    assert_eq!(c3_page.stop_state, StopState::S2Exhausted);
    assert_eq!(c3_page.reply.retrieval_depth, 3200);

    let head_pair = builder("c5-head-pair", numbered_candidates(2), false);
    let head_page = serve_public_page(&head_pair, request(0, fixture.coincidences.c5_top_k))
        .expect("C5 head-pair page");
    assert_eq!(head_page.shown(), 1);
    assert_eq!(head_page.reply.effective_target, 2);
    assert_eq!(head_page.total_at_stop(), 2);
}
