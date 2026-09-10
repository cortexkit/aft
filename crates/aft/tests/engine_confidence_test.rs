use std::cell::Cell;
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod comparator {
    pub use aft::commands::semantic_search::comparator::*;
}
mod evidence_descriptor {
    pub use aft::commands::semantic_search::evidence_descriptor::*;
}
mod plan_table {
    pub use aft::commands::semantic_search::plan_table::*;
}

#[path = "../src/commands/semantic_search/blocks.rs"]
mod blocks;
#[path = "../src/commands/semantic_search/confidence.rs"]
mod confidence;
#[path = "../src/commands/semantic_search/scoring.rs"]
mod scoring;

use blocks::{
    BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate, PageRequest, BLOCK_DEPTHS,
    MAX_BLOCK_DEPTH,
};
use comparator::SymbolOffsetRange;
use confidence::{
    Confidence, ConfidenceBranch, ConfidenceCandidate, ConfidenceDecision, ConfidenceEngine,
    FLAT_HEAD_LINE,
};
use evidence_descriptor::EvidenceDescriptor;
use plan_table::SearchLaneKind;
use scoring::{LaneScoringRule, ScoringPolicy};

#[derive(Debug, Deserialize)]
struct Fixture {
    schema: u64,
    page_local_head: PageLocalHeadFixture,
    margin: MarginFixture,
    descriptor_pairs: Vec<DescriptorPairFixture>,
    head_pair: HeadPairFixture,
}

#[derive(Debug, Deserialize)]
struct PageLocalHeadFixture {
    result_count: usize,
    later_offset: usize,
    out_of_range_offset: usize,
}

#[derive(Debug, Deserialize)]
struct MarginFixture {
    within: [f32; 2],
    calibration_flip: [f32; 2],
    separated: [f32; 2],
    calibrated_margin: f32,
    calibrated_model_id: String,
}

#[derive(Debug, Deserialize)]
struct DescriptorPairFixture {
    label: String,
    winner_path: PathBuf,
    loser_path: PathBuf,
    winner_range: [usize; 2],
    loser_range: [usize; 2],
    winner_exact_form: bool,
    loser_exact_form: bool,
    winner_generated: bool,
    loser_generated: bool,
}

#[derive(Debug, Deserialize)]
struct HeadPairFixture {
    filtered_prefix: usize,
    depth_one: usize,
    depth_max: usize,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn fixture() -> Fixture {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/confidence/cases.json"
    ))
    .expect("confidence fixture must be valid JSON");
    assert_eq!(fixture.schema, 1);
    fixture
}

fn engine() -> ConfidenceEngine {
    ConfidenceEngine::start_at_workspace_root(&workspace_root())
        .expect("shipped confidence threshold must load")
}

fn key(label: &str) -> CanonicalListKey {
    CanonicalListKey {
        project_root: PathBuf::from(format!("/virtual/confidence/{label}")),
        snapshot_generation: "confidence-generation-1".to_string(),
        normalized_query: label.to_string(),
        include_tests: false,
    }
}

fn lane(kind: SearchLaneKind, candidates: Vec<LaneCandidate>) -> CanonicalLane {
    CanonicalLane::new(kind, candidates).expect("valid confidence lane")
}

fn equal_lane_policy() -> ScoringPolicy {
    ScoringPolicy::new(
        LaneScoringRule {
            weight: 1.0,
            rrf_constant: 0.0,
            plan_order_index: SearchLaneKind::Lexical.default_plan_order_index(),
        },
        LaneScoringRule {
            weight: 1.0,
            rrf_constant: 0.0,
            plan_order_index: SearchLaneKind::Semantic.default_plan_order_index(),
        },
    )
    .expect("equal-weight confidence policy")
}

fn weighted_head_pair_policy(filtered_prefix: usize) -> ScoringPolicy {
    ScoringPolicy::new(
        LaneScoringRule {
            weight: filtered_prefix as f32 + 1.0,
            rrf_constant: 0.0,
            plan_order_index: SearchLaneKind::Lexical.default_plan_order_index(),
        },
        LaneScoringRule {
            weight: 1.0,
            rrf_constant: 0.0,
            plan_order_index: SearchLaneKind::Semantic.default_plan_order_index(),
        },
    )
    .expect("head-pair confidence policy")
}

fn non_exact_candidate(
    path: impl Into<PathBuf>,
    range: Option<SymbolOffsetRange>,
    exact_form: bool,
    generated: bool,
    is_test: bool,
) -> LaneCandidate {
    LaneCandidate::non_exact(
        path,
        range,
        EvidenceDescriptor::for_non_exact(exact_form, generated),
        0.8,
        is_test,
    )
}

fn assert_high_without_line(decision: &ConfidenceDecision) {
    assert_eq!(decision.confidence, Some(Confidence::High));
    assert_eq!(decision.flat_head_line, None);
}

fn assert_low_with_line(decision: &ConfidenceDecision) {
    assert_eq!(decision.confidence, Some(Confidence::Low));
    assert_eq!(decision.flat_head_line, Some(FLAT_HEAD_LINE));
}

#[derive(Debug)]
struct ProbeCandidate {
    evidence: EvidenceDescriptor,
    fusion_score: Option<f32>,
    score_reads: Cell<usize>,
    panic_on_score: bool,
}

impl ProbeCandidate {
    fn exact(evidence: EvidenceDescriptor) -> Self {
        Self {
            evidence,
            fusion_score: None,
            score_reads: Cell::new(0),
            panic_on_score: true,
        }
    }

    fn non_exact(score: f32) -> Self {
        Self {
            evidence: EvidenceDescriptor::for_non_exact(false, false),
            fusion_score: Some(score),
            score_reads: Cell::new(0),
            panic_on_score: false,
        }
    }

    fn non_exact_score_panics(score: f32) -> Self {
        Self {
            evidence: EvidenceDescriptor::for_non_exact(false, false),
            fusion_score: Some(score),
            score_reads: Cell::new(0),
            panic_on_score: true,
        }
    }
}

impl ConfidenceCandidate for ProbeCandidate {
    fn evidence_descriptor(&self) -> &EvidenceDescriptor {
        &self.evidence
    }

    fn frozen_fusion_score(&self) -> Option<f32> {
        self.score_reads.set(self.score_reads.get() + 1);
        assert!(!self.panic_on_score, "exact-tier score was accessed");
        self.fusion_score
    }
}

#[test]
fn confidence_uses_the_canonical_head_on_every_page_and_empty_page() {
    let fixture = fixture();
    let exact = (0..fixture.page_local_head.result_count)
        .map(|position| {
            let evidence = if position == 0 {
                EvidenceDescriptor::for_definition(true, false)
            } else {
                EvidenceDescriptor::for_e1(1, true, false)
            };
            LaneCandidate::exact(format!("result-{position:03}.rs"), None, evidence, false)
        })
        .collect();
    let builder = BlockBuilder::new(
        key("canonical-head"),
        ScoringPolicy::empty(),
        vec![lane(SearchLaneKind::Exact, exact)],
    )
    .expect("canonical-head builder");

    let top_one = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 1,
        })
        .expect("topK one reply");
    let top_ten = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 10,
        })
        .expect("topK ten reply");
    let later = builder
        .build_for_request(PageRequest {
            offset: fixture.page_local_head.later_offset,
            top_k: 10,
        })
        .expect("later page reply");
    let empty = builder
        .build_for_request(PageRequest {
            offset: fixture.page_local_head.out_of_range_offset,
            top_k: 10,
        })
        .expect("out-of-range page reply");
    let depth_400 = builder
        .build_at_depth(400)
        .expect("depth-400 reference reply");

    let confidence = engine();
    for reply in [&top_one, &top_ten, &later, &empty, &depth_400] {
        let decision = confidence.evaluate_reply(reply).expect("confidence");
        assert_high_without_line(&decision);
    }

    assert_eq!(top_one.page.len(), 1);
    assert_eq!(top_ten.page.len(), 10);
    assert_eq!(later.page.len(), 10);
    assert!(empty.page.is_empty());
    assert_eq!(
        empty.canonical_list.len(),
        fixture.page_local_head.result_count
    );

    let local_later_head = confidence
        .evaluate_candidates(&later.page)
        .expect("page-local diagnostic");
    assert_low_with_line(&local_later_head);
    let empty_page_head = confidence
        .evaluate_candidates(&empty.page)
        .expect("empty page-local diagnostic");
    assert_eq!(empty_page_head.confidence, None);
    assert_eq!(empty_page_head.flat_head_line, None);
}

fn assert_descriptor_pair(label: &str) {
    let pair = fixture()
        .descriptor_pairs
        .into_iter()
        .find(|pair| pair.label == label)
        .unwrap_or_else(|| panic!("missing descriptor pair {label}"));
    let winner_range = Some(SymbolOffsetRange::new(
        pair.winner_range[0],
        pair.winner_range[1],
    ));
    let loser_range = Some(SymbolOffsetRange::new(
        pair.loser_range[0],
        pair.loser_range[1],
    ));
    assert_ne!(
        (pair.winner_path.clone(), winner_range),
        (pair.loser_path.clone(), loser_range),
        "{} must contain two distinct (file, symbol range) identities",
        pair.label
    );

    let winner = non_exact_candidate(
        pair.winner_path.clone(),
        winner_range,
        pair.winner_exact_form,
        pair.winner_generated,
        false,
    );
    let loser = non_exact_candidate(
        pair.loser_path.clone(),
        loser_range,
        pair.loser_exact_form,
        pair.loser_generated,
        false,
    );
    let lexical = lane(SearchLaneKind::Lexical, vec![winner.clone(), loser.clone()]);
    let semantic = lane(SearchLaneKind::Semantic, vec![loser, winner]);
    let reply = BlockBuilder::new(
        key(&pair.label),
        equal_lane_policy(),
        vec![lexical, semantic],
    )
    .expect("descriptor-pair builder")
    .build_for_request(PageRequest {
        offset: 0,
        top_k: 2,
    })
    .expect("descriptor-pair reply");

    let entries = reply.canonical_list.entries().collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        2,
        "{} must retain both candidates",
        pair.label
    );
    assert_eq!(
        entries[0].result.path, pair.winner_path,
        "{} rank 1",
        pair.label
    );
    assert_eq!(
        entries[1].result.path, pair.loser_path,
        "{} rank 2",
        pair.label
    );
    assert_eq!(
        entries[0].result.fusion_score,
        entries[1].result.fusion_score
    );
    assert_eq!(entries[0].result.lane_score, entries[1].result.lane_score);

    let decision = engine().evaluate_reply(&reply).expect("pair confidence");
    assert_high_without_line(&decision);
    assert_eq!(decision.branch, ConfidenceBranch::NonExactDescriptor);
}

#[test]
fn exact_form_descriptor_pair_favorable_path_is_high() {
    assert_descriptor_pair("P");
}

#[test]
fn exact_form_descriptor_pair_adverse_path_is_high() {
    assert_descriptor_pair("P-prime-adverse-path");
}

#[test]
fn generated_descriptor_pair_favorable_path_is_high() {
    assert_descriptor_pair("Q");
}

#[test]
fn generated_descriptor_pair_adverse_path_is_high() {
    assert_descriptor_pair("Q-prime-adverse-path");
}

#[test]
fn exact_head_rules_and_counting_never_read_scores() {
    let confidence = engine();

    let empty: Vec<ProbeCandidate> = Vec::new();
    let decision = confidence
        .evaluate_candidates(&empty)
        .expect("empty confidence");
    assert_eq!(decision.confidence, None);
    assert_eq!(decision.flat_head_line, None);
    assert_eq!(decision.branch, ConfidenceBranch::Empty);

    for evidence in [
        EvidenceDescriptor::for_definition(true, false),
        EvidenceDescriptor::for_e1(1, true, false),
        EvidenceDescriptor::for_anchored(12, 3, true, false),
        EvidenceDescriptor::for_e2(2, true, false),
    ] {
        let singleton = [ProbeCandidate::exact(evidence)];
        let decision = confidence
            .evaluate_candidates(&singleton)
            .expect("exact singleton confidence");
        assert_high_without_line(&decision);
        assert_eq!(decision.branch, ConfidenceBranch::SingletonExact);
        assert_eq!(singleton[0].score_reads.get(), 0);
    }

    let singleton_non_exact = [ProbeCandidate::non_exact_score_panics(0.5)];
    let decision = confidence
        .evaluate_candidates(&singleton_non_exact)
        .expect("non-exact singleton confidence");
    assert_eq!(decision.confidence, Some(Confidence::Low));
    assert_eq!(decision.flat_head_line, None);
    assert_eq!(decision.branch, ConfidenceBranch::SingletonNonExact);
    assert_eq!(singleton_non_exact[0].score_reads.get(), 0);

    let exact_cases = [
        (
            ProbeCandidate::exact(EvidenceDescriptor::for_e1(2, true, false)),
            ProbeCandidate::exact(EvidenceDescriptor::for_e1(2, false, true)),
            Confidence::Low,
        ),
        (
            ProbeCandidate::exact(EvidenceDescriptor::for_e2(2, true, false)),
            ProbeCandidate::exact(EvidenceDescriptor::for_e2(2, false, true)),
            Confidence::Low,
        ),
        (
            ProbeCandidate::exact(EvidenceDescriptor::for_anchored(20, 4, true, false)),
            ProbeCandidate::exact(EvidenceDescriptor::for_anchored(20, 4, false, true)),
            Confidence::Low,
        ),
        (
            ProbeCandidate::exact(EvidenceDescriptor::for_definition(false, true)),
            ProbeCandidate::exact(EvidenceDescriptor::for_e1(100, true, false)),
            Confidence::High,
        ),
        (
            ProbeCandidate::exact(EvidenceDescriptor::for_e1(3, false, true)),
            ProbeCandidate::exact(EvidenceDescriptor::for_e1(2, true, false)),
            Confidence::High,
        ),
    ];

    for (first, second, expected) in exact_cases {
        let candidates = [first, second];
        let decision = confidence
            .evaluate_candidates(&candidates)
            .expect("exact pair confidence");
        assert_eq!(decision.confidence, Some(expected));
        if expected == Confidence::Low {
            assert_eq!(decision.flat_head_line, Some(FLAT_HEAD_LINE));
        } else {
            assert_eq!(decision.flat_head_line, None);
        }
        assert_eq!(decision.branch, ConfidenceBranch::ExactEvidence);
        assert_eq!(candidates[0].score_reads.get(), 0);
        assert_eq!(candidates[1].score_reads.get(), 0);
    }

    let mixed = [
        ProbeCandidate::exact(EvidenceDescriptor::for_e1(1, true, false)),
        ProbeCandidate::non_exact_score_panics(0.5),
    ];
    let decision = confidence
        .evaluate_candidates(&mixed)
        .expect("exact-over-non-exact confidence");
    assert_high_without_line(&decision);
    assert_eq!(decision.branch, ConfidenceBranch::ExactOverNonExact);
    assert_eq!(mixed[0].score_reads.get(), 0);
    assert_eq!(mixed[1].score_reads.get(), 0);
}

#[test]
fn non_exact_margin_uses_only_ranks_one_and_two() {
    let fixture = fixture();
    let confidence = engine();

    let equal = [
        ProbeCandidate::non_exact(0.5),
        ProbeCandidate::non_exact(0.5),
    ];
    assert_low_with_line(
        &confidence
            .evaluate_candidates(&equal)
            .expect("equal-score confidence"),
    );

    let within = [
        ProbeCandidate::non_exact(fixture.margin.within[0]),
        ProbeCandidate::non_exact(fixture.margin.within[1]),
    ];
    assert_low_with_line(
        &confidence
            .evaluate_candidates(&within)
            .expect("within-margin confidence"),
    );

    let three_flat = [
        ProbeCandidate::non_exact(fixture.margin.within[0]),
        ProbeCandidate::non_exact(fixture.margin.within[1]),
        ProbeCandidate::non_exact_score_panics(0.1),
    ];
    assert!(three_flat[2].fusion_score < three_flat[1].fusion_score);
    let decision = confidence
        .evaluate_candidates(&three_flat)
        .expect("three-result flat-head confidence");
    assert_low_with_line(&decision);
    assert_eq!(three_flat[2].score_reads.get(), 0);

    let three_separated = [
        ProbeCandidate::non_exact(fixture.margin.separated[0]),
        ProbeCandidate::non_exact(fixture.margin.separated[1]),
        ProbeCandidate::non_exact_score_panics(fixture.margin.separated[1]),
    ];
    assert_eq!(
        three_separated[2].fusion_score,
        three_separated[1].fusion_score
    );
    let decision = confidence
        .evaluate_candidates(&three_separated)
        .expect("three-result separated confidence");
    assert_high_without_line(&decision);
    assert_eq!(decision.branch, ConfidenceBranch::NonExactMargin);
    assert_eq!(three_separated[2].score_reads.get(), 0);

    assert!(fixture.margin.calibration_flip[0] > fixture.margin.calibration_flip[1]);
    assert!(fixture.margin.calibrated_margin > 0.05);
    assert!(!fixture.margin.calibrated_model_id.is_empty());
}

#[test]
fn head_pair_escalation_singleton_states_and_tier_zero_cost_are_total() {
    let fixture = fixture();
    assert_eq!(fixture.head_pair.depth_one, BLOCK_DEPTHS[1]);
    assert_eq!(fixture.head_pair.depth_max, MAX_BLOCK_DEPTH);

    let mut lexical = (0..fixture.head_pair.filtered_prefix)
        .map(|position| {
            non_exact_candidate(
                format!("tests/filtered-{position:03}.rs"),
                None,
                false,
                false,
                true,
            )
        })
        .collect::<Vec<_>>();
    lexical.push(non_exact_candidate(
        "y-tier-one.rs",
        None,
        false,
        false,
        false,
    ));
    let semantic = vec![non_exact_candidate(
        "x-tier-zero.rs",
        None,
        true,
        false,
        false,
    )];
    let h1_builder = BlockBuilder::new(
        key("h1-head-pair"),
        weighted_head_pair_policy(fixture.head_pair.filtered_prefix),
        vec![
            lane(SearchLaneKind::Lexical, lexical),
            lane(SearchLaneKind::Semantic, semantic),
        ],
    )
    .expect("H1 builder");
    let h1_top_one = h1_builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 1,
        })
        .expect("H1 topK one");
    let h1_depth_400 = h1_builder
        .build_at_depth(fixture.head_pair.depth_one)
        .expect("H1 depth 400");

    assert_eq!(h1_top_one.canonical_list.block(0).unwrap().entries.len(), 1);
    assert_eq!(h1_top_one.canonical_list.block(1).unwrap().entries.len(), 1);
    let h1_entries = h1_top_one.canonical_list.entries().collect::<Vec<_>>();
    assert_eq!(h1_entries.len(), 2);
    assert_eq!(h1_entries[0].result.path, PathBuf::from("x-tier-zero.rs"));
    assert_eq!(h1_entries[1].result.path, PathBuf::from("y-tier-one.rs"));
    assert_eq!(
        h1_depth_400
            .canonical_list
            .entries()
            .nth(1)
            .unwrap()
            .result
            .path,
        PathBuf::from("y-tier-one.rs")
    );
    assert_eq!(
        h1_entries[0].result.fusion_score,
        h1_entries[1].result.fusion_score
    );
    assert_eq!(h1_top_one.retrieval_depth, fixture.head_pair.depth_one);
    assert_eq!(
        serde_json::to_vec(&h1_top_one.canonical_list.stability_units()).unwrap(),
        serde_json::to_vec(&h1_depth_400.canonical_list.stability_units()).unwrap()
    );
    for reply in [&h1_top_one, &h1_depth_400] {
        assert_high_without_line(&engine().evaluate_reply(reply).expect("H1 confidence"));
    }

    let h2_builder = BlockBuilder::new(
        key("h2-exhausted-singleton"),
        equal_lane_policy(),
        vec![lane(
            SearchLaneKind::Semantic,
            vec![non_exact_candidate("only.rs", None, false, false, false)],
        )],
    )
    .expect("H2 builder");
    let h2 = h2_builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 1,
        })
        .expect("H2 reply");
    assert_eq!(h2.canonical_list.len(), 1);
    assert!(h2.lanes_exhausted);
    let h2_decision = engine().evaluate_reply(&h2).expect("H2 confidence");
    assert_eq!(h2_decision.confidence, Some(Confidence::Low));
    assert_eq!(h2_decision.flat_head_line, None);

    let mut capped = vec![non_exact_candidate(
        "only-visible.rs",
        None,
        false,
        false,
        false,
    )];
    capped.extend((1..fixture.head_pair.depth_max).map(|position| {
        non_exact_candidate(
            format!("tests/capped-filtered-{position:04}.rs"),
            None,
            false,
            false,
            true,
        )
    }));
    capped.push(non_exact_candidate(
        "unconsumed-after-cap.rs",
        None,
        false,
        false,
        false,
    ));
    let h3_builder = BlockBuilder::new(
        key("h3-depth-cap-singleton"),
        equal_lane_policy(),
        vec![lane(SearchLaneKind::Semantic, capped)],
    )
    .expect("H3 builder");
    let h3 = h3_builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 1,
        })
        .expect("H3 reply");
    assert_eq!(h3.canonical_list.len(), 1);
    assert_eq!(h3.retrieval_depth, fixture.head_pair.depth_max);
    assert_eq!(h3.depth_tier, BLOCK_DEPTHS.len() - 1);
    assert!(!h3.lanes_exhausted);
    assert!(h3.effective_target > h3.canonical_list.len() as u64);
    let h3_decision = engine().evaluate_reply(&h3).expect("H3 confidence");
    assert_eq!(h3_decision.confidence, Some(Confidence::Low));
    assert_eq!(h3_decision.flat_head_line, None);

    let mut tier_zero = vec![
        non_exact_candidate("first.rs", None, true, false, false),
        non_exact_candidate("second.rs", None, false, false, false),
    ];
    tier_zero.extend((2..=BLOCK_DEPTHS[0]).map(|position| {
        non_exact_candidate(format!("tail-{position:03}.rs"), None, false, false, false)
    }));
    let tier_zero_reply = BlockBuilder::new(
        key("head-ready-at-tier-zero"),
        equal_lane_policy(),
        vec![lane(SearchLaneKind::Semantic, tier_zero)],
    )
    .expect("tier-zero builder")
    .build_for_request(PageRequest {
        offset: 0,
        top_k: 1,
    })
    .expect("tier-zero reply");
    assert_eq!(tier_zero_reply.effective_target, 2);
    assert_eq!(tier_zero_reply.retrieval_depth, BLOCK_DEPTHS[0]);
    assert_eq!(
        tier_zero_reply
            .lane_enumeration_counts
            .get(&SearchLaneKind::Semantic),
        Some(&BLOCK_DEPTHS[0])
    );
}

#[test]
fn provenance_is_structurally_unavailable_to_confidence() {
    struct StubbedProvenanceCandidate {
        inner: ProbeCandidate,
        lane_position_reads: Cell<usize>,
    }

    impl StubbedProvenanceCandidate {
        fn lane_positions(&self) -> ! {
            self.lane_position_reads
                .set(self.lane_position_reads.get() + 1);
            panic!("confidence must not read lane_positions")
        }
    }

    impl ConfidenceCandidate for StubbedProvenanceCandidate {
        fn evidence_descriptor(&self) -> &EvidenceDescriptor {
            &self.inner.evidence
        }

        fn frozen_fusion_score(&self) -> Option<f32> {
            self.inner.frozen_fusion_score()
        }
    }

    let candidates = [
        StubbedProvenanceCandidate {
            inner: ProbeCandidate::non_exact(0.5),
            lane_position_reads: Cell::new(0),
        },
        StubbedProvenanceCandidate {
            inner: ProbeCandidate::non_exact(0.4),
            lane_position_reads: Cell::new(0),
        },
    ];
    let decision = engine()
        .evaluate_candidates(&candidates)
        .expect("stubbed-provenance confidence");
    assert_high_without_line(&decision);
    assert_eq!(candidates[0].lane_position_reads.get(), 0);
    assert_eq!(candidates[1].lane_position_reads.get(), 0);
}
