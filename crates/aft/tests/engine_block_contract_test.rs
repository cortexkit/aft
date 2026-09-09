use std::path::PathBuf;

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
#[path = "../src/commands/semantic_search/scoring.rs"]
mod scoring;

use blocks::{
    BlockBuilder, CanonicalLane, CanonicalListKey, ContributionDisposition, LaneCandidate,
    PageRequest, BLOCK_DEPTHS, MAX_BLOCK_DEPTH,
};
use comparator::CandidateResult;
use evidence_descriptor::EvidenceDescriptor;
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use scoring::{
    admitted_contributions, freeze_non_exact_scores, LaneContribution, ScoringError, ScoringPolicy,
};

#[derive(Debug, Deserialize)]
struct Fixture {
    depths: [usize; 5],
    cross_lane: CrossLaneFixture,
    frozen_scoring: FrozenScoringFixture,
    boundaries: BoundaryFixture,
    exact_only_count: usize,
    mixed_exact_count: usize,
    n_eff: EffectiveTargetFixture,
}

#[derive(Debug, Deserialize)]
struct CrossLaneFixture {
    semantic_position: usize,
    lexical_position: usize,
    expected_b0_size: usize,
    direct_offset: usize,
    direct_top_k: usize,
}

#[derive(Debug, Deserialize)]
struct FrozenScoringFixture {
    semantic_position_a: usize,
    lexical_position_a: usize,
    shallow_depth: usize,
    deep_depth: usize,
}

#[derive(Debug, Deserialize)]
struct BoundaryFixture {
    dense_b0_size: usize,
    sparse_b0_size: usize,
    sparse_b1_index: usize,
}

#[derive(Debug, Deserialize)]
struct EffectiveTargetFixture {
    top_k: usize,
    expected_effective_target: u64,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/blocks/tier-attribution.json"
    ))
    .expect("block fixture must be valid JSON")
}

fn key() -> CanonicalListKey {
    CanonicalListKey {
        project_root: PathBuf::from("/virtual/block-fixture"),
        snapshot_generation: "generation-17".to_string(),
        normalized_query: "block frozen query".to_string(),
        include_tests: false,
    }
}

fn policy() -> ScoringPolicy {
    ScoringPolicy::from_plan_table(&PlanTable::running_table(), SearchShape::NaturalLanguage)
        .expect("running natural-language scoring policy")
}

fn path_policy() -> ScoringPolicy {
    ScoringPolicy::from_plan_table(&PlanTable::running_table(), SearchShape::Path)
        .expect("running path scoring policy")
}

fn non_exact(path: impl Into<PathBuf>, raw_score: f32) -> LaneCandidate {
    LaneCandidate::non_exact(
        path,
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        raw_score,
        false,
    )
}

fn filtered(path: impl Into<PathBuf>) -> LaneCandidate {
    LaneCandidate::non_exact(
        path,
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.01,
        true,
    )
}

fn lane(kind: SearchLaneKind, candidates: Vec<LaneCandidate>) -> CanonicalLane {
    CanonicalLane::new(kind, candidates).expect("canonical lane fixture")
}

fn find<'a>(reply: &'a blocks::BlockReply, path: &str) -> &'a blocks::BlockEntry {
    reply
        .canonical_list
        .entries()
        .find(|entry| entry.result.path == PathBuf::from(path))
        .unwrap_or_else(|| panic!("missing block entry {path}"))
}

fn cross_lane_lanes(fixture: &Fixture) -> (CanonicalLane, CanonicalLane) {
    let a = PathBuf::from("target-a.rs");
    let mut semantic = Vec::new();
    let mut shared_paths = Vec::new();
    for position in 0..199 {
        if position == fixture.cross_lane.semantic_position {
            semantic.push(non_exact(a.clone(), 0.8));
        } else {
            let path = PathBuf::from(format!("shared-{position:03}.rs"));
            shared_paths.push(path.clone());
            semantic.push(non_exact(path, 0.5));
        }
    }

    let mut lexical = shared_paths
        .into_iter()
        .map(|path| non_exact(path, 0.5))
        .collect::<Vec<_>>();
    assert_eq!(lexical.len(), 198);
    lexical.push(non_exact("lexical-b0-extra.rs", 0.4));
    lexical.push(filtered("tests/filtered-at-199.rs"));
    while lexical.len() < fixture.cross_lane.lexical_position {
        let position = lexical.len();
        lexical.push(non_exact(format!("tier-one-{position:03}.rs"), 0.2));
    }
    lexical.push(non_exact(a, 100.0));
    while lexical.len() < 400 {
        let position = lexical.len();
        lexical.push(non_exact(format!("tail-{position:03}.rs"), 0.1));
    }

    (
        lane(SearchLaneKind::Lexical, lexical),
        lane(SearchLaneKind::Semantic, semantic),
    )
}

#[test]
fn cross_lane_tier_is_minimum_over_all_lanes_and_completion_orders() {
    let fixture = fixture();
    assert_eq!(fixture.depths, BLOCK_DEPTHS);
    assert_eq!(MAX_BLOCK_DEPTH, *BLOCK_DEPTHS.last().unwrap());
    let (lexical, semantic) = cross_lane_lanes(&fixture);

    let lexical_first = BlockBuilder::new(key(), policy(), vec![lexical.clone(), semantic.clone()])
        .expect("lexical-first builder")
        .build_at_depth(400)
        .expect("lexical-first depth-400 reply");
    let semantic_first = BlockBuilder::new(key(), policy(), vec![semantic, lexical])
        .expect("semantic-first builder")
        .build_at_depth(400)
        .expect("semantic-first depth-400 reply");

    assert_eq!(
        lexical_first.canonical_list.block(0).unwrap().entries.len(),
        fixture.cross_lane.expected_b0_size
    );
    assert_eq!(find(&lexical_first, "target-a.rs").tier_index, 0);
    assert_eq!(find(&semantic_first, "target-a.rs").tier_index, 0);
    assert_eq!(
        serde_json::to_vec(&lexical_first).unwrap(),
        serde_json::to_vec(&semantic_first).unwrap(),
        "lane completion order must not affect the whole reply at one depth"
    );

    let (lexical, semantic) = cross_lane_lanes(&fixture);
    let direct_lexical_first =
        BlockBuilder::new(key(), policy(), vec![lexical.clone(), semantic.clone()])
            .unwrap()
            .build_for_request(PageRequest {
                offset: fixture.cross_lane.direct_offset,
                top_k: fixture.cross_lane.direct_top_k,
            })
            .unwrap();
    let direct_semantic_first = BlockBuilder::new(key(), policy(), vec![semantic, lexical])
        .unwrap()
        .build_for_request(PageRequest {
            offset: fixture.cross_lane.direct_offset,
            top_k: fixture.cross_lane.direct_top_k,
        })
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&direct_lexical_first.page_stability_units()).unwrap(),
        serde_json::to_vec(&direct_semantic_first.page_stability_units()).unwrap()
    );
}

fn frozen_scoring_lanes(fixture: &Fixture) -> (CanonicalLane, CanonicalLane) {
    let mut semantic = Vec::new();
    for position in 0..220 {
        let candidate = match position {
            0 => non_exact("candidate-b.rs", 0.8),
            position if position == fixture.frozen_scoring.semantic_position_a => {
                non_exact("candidate-a.rs", 0.4)
            }
            _ => non_exact(format!("semantic-{position:03}.rs"), 0.05),
        };
        semantic.push(candidate);
    }

    let mut lexical = Vec::new();
    lexical.push(non_exact("candidate-b.rs", 0.8));
    while lexical.len() < fixture.frozen_scoring.lexical_position_a {
        let position = lexical.len();
        lexical.push(non_exact(format!("lexical-{position:03}.rs"), 0.05));
    }
    lexical.push(non_exact("candidate-a.rs", 100.0));
    while lexical.len() < 400 {
        let position = lexical.len();
        lexical.push(non_exact(format!("lexical-{position:03}.rs"), 0.01));
    }

    (
        lane(SearchLaneKind::Lexical, lexical),
        lane(SearchLaneKind::Semantic, semantic),
    )
}

#[test]
fn admitted_scoring_is_frozen_at_the_candidates_own_tier_depth() {
    let fixture = fixture();
    let (lexical, semantic) = frozen_scoring_lanes(&fixture);
    let builder = BlockBuilder::new(key(), policy(), vec![lexical, semantic]).unwrap();
    let shallow = builder
        .build_at_depth(fixture.frozen_scoring.shallow_depth)
        .unwrap();
    let deep = builder
        .build_at_depth(fixture.frozen_scoring.deep_depth)
        .unwrap();

    assert_eq!(
        shallow.lane_enumeration_counts[&SearchLaneKind::Lexical],
        fixture.frozen_scoring.shallow_depth
    );
    let shallow_a = find(&shallow, "candidate-a.rs");
    let deep_a = find(&deep, "candidate-a.rs");
    let deep_b = find(&deep, "candidate-b.rs");
    assert!(deep_b.r3_order_index < deep_a.r3_order_index);
    assert_eq!(shallow_a.result.fusion_score, deep_a.result.fusion_score);
    assert_eq!(shallow_a.result.lane_score, Some(0.4));
    assert_eq!(shallow_a.result.lane_score, deep_a.result.lane_score);
    assert_eq!(shallow_a.result.evidence, deep_a.result.evidence);
    assert_eq!(shallow_a.r3_order_index, deep_a.r3_order_index);
    assert_eq!(
        shallow_a
            .admitted_contributions
            .iter()
            .map(|contribution| contribution.lane)
            .collect::<Vec<_>>(),
        vec![SearchLaneKind::Semantic]
    );
    assert_eq!(
        deep_a
            .admitted_contributions
            .iter()
            .map(|contribution| contribution.lane)
            .collect::<Vec<_>>(),
        vec![SearchLaneKind::Semantic]
    );
    assert_eq!(
        shallow_a
            .lane_attribution
            .iter()
            .find(|value| value.lane == SearchLaneKind::Lexical),
        None
    );
    assert_eq!(
        deep_a
            .lane_attribution
            .iter()
            .find(|value| value.lane == SearchLaneKind::Lexical)
            .map(|value| (value.position, value.disposition)),
        Some((
            fixture.frozen_scoring.lexical_position_a,
            ContributionDisposition::NotAdmitted
        ))
    );

    let shallow_b0 = shallow
        .canonical_list
        .block(0)
        .unwrap()
        .entries
        .iter()
        .map(blocks::BlockEntry::stability_unit)
        .collect::<Vec<_>>();
    let deep_b0 = deep
        .canonical_list
        .block(0)
        .unwrap()
        .entries
        .iter()
        .map(blocks::BlockEntry::stability_unit)
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_vec(&shallow_b0).unwrap(),
        serde_json::to_vec(&deep_b0).unwrap()
    );
}

#[test]
fn admitted_set_cutoff_and_score_totality_are_explicit() {
    let fixture = fixture();
    let contributions = vec![
        LaneContribution {
            lane: SearchLaneKind::Lexical,
            position: fixture.frozen_scoring.lexical_position_a,
            raw_score: 100.0,
        },
        LaneContribution {
            lane: SearchLaneKind::Semantic,
            position: fixture.frozen_scoring.semantic_position_a,
            raw_score: 0.4,
        },
    ];
    let admitted = admitted_contributions(&contributions, 200).unwrap();
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].lane, SearchLaneKind::Semantic);
    let scores = freeze_non_exact_scores(&contributions, 200, &policy()).unwrap();
    assert_eq!(scores.lane_score, 0.4);
    assert_eq!(scores.best_lane, SearchLaneKind::Semantic);
    assert_eq!(
        freeze_non_exact_scores(&[], 200, &policy()),
        Err(ScoringError::EmptyAdmittedSet)
    );
}

fn dense_boundary_lanes(count: usize) -> Vec<CanonicalLane> {
    let semantic = (0..count)
        .map(|position| non_exact(format!("dense-{position:03}.rs"), 0.1))
        .collect::<Vec<_>>();
    let mut lexical = (0..count)
        .map(|position| filtered(format!("tests/dense-filtered-{position:03}.rs")))
        .collect::<Vec<_>>();
    lexical.push(non_exact("000-tier-one-high.rs", 1_000.0));
    vec![
        lane(SearchLaneKind::Semantic, semantic),
        lane(SearchLaneKind::Lexical, lexical),
    ]
}

#[test]
fn blocks_preserve_dense_and_sparse_boundaries_without_global_resort() {
    let fixture = fixture();
    let dense = BlockBuilder::new(
        key(),
        path_policy(),
        dense_boundary_lanes(fixture.boundaries.dense_b0_size),
    )
    .unwrap()
    .build_at_depth(400)
    .unwrap();
    assert_eq!(
        dense.canonical_list.block(0).unwrap().entries.len(),
        fixture.boundaries.dense_b0_size
    );
    let escalated = find(&dense, "000-tier-one-high.rs");
    assert_eq!(escalated.tier_index, 1);
    assert_eq!(escalated.r3_order_index, fixture.boundaries.dense_b0_size);
    assert!(
        escalated.result.fusion_score
            > dense.canonical_list.block(0).unwrap().entries[0]
                .result
                .fusion_score,
        "the tier-one candidate must be strong enough to expose a global-resort mutation"
    );
    let dense_position = dense
        .canonical_list
        .entries()
        .position(|entry| entry.result.path == PathBuf::from("000-tier-one-high.rs"));

    let sparse_semantic = (0..fixture.boundaries.sparse_b0_size)
        .map(|position| non_exact(format!("sparse-{position:03}.rs"), 0.01))
        .collect::<Vec<_>>();
    let mut sparse_lexical = (0..fixture.boundaries.dense_b0_size)
        .map(|position| filtered(format!("tests/sparse-filtered-{position:03}.rs")))
        .collect::<Vec<_>>();
    sparse_lexical.push(non_exact("000-sparse-tier-one-high.rs", 5_000.0));
    let sparse = BlockBuilder::new(
        key(),
        path_policy(),
        vec![
            lane(SearchLaneKind::Semantic, sparse_semantic),
            lane(SearchLaneKind::Lexical, sparse_lexical),
        ],
    )
    .unwrap()
    .build_at_depth(400)
    .unwrap();
    assert_eq!(
        sparse.canonical_list.block(0).unwrap().entries.len(),
        fixture.boundaries.sparse_b0_size
    );
    let sparse_escalated = find(&sparse, "000-sparse-tier-one-high.rs");
    assert_eq!(
        sparse_escalated.r3_order_index,
        fixture.boundaries.sparse_b1_index
    );
    assert!(
        sparse_escalated.result.fusion_score
            > sparse.canonical_list.block(0).unwrap().entries[0]
                .result
                .fusion_score,
        "the sparse boundary must be observable under a global-resort mutation"
    );
    let sparse_position = sparse
        .canonical_list
        .entries()
        .position(|entry| entry.result.path == PathBuf::from("000-sparse-tier-one-high.rs"));
    assert_eq!(
        (dense_position, sparse_position),
        (
            Some(fixture.boundaries.dense_b0_size),
            Some(fixture.boundaries.sparse_b1_index)
        ),
        "neither dense nor sparse block boundaries may be crossed by a stronger later-tier result"
    );
}

#[test]
fn effective_target_is_two_but_never_anticipates_a_later_page() {
    let fixture = fixture();
    let mut one_then_one = vec![non_exact("head.rs", 1.0)];
    one_then_one.extend((1..200).map(|position| filtered(format!("tests/gap-{position}.rs"))));
    one_then_one.push(non_exact("second.rs", 0.5));
    one_then_one.push(non_exact("unconsumed-third.rs", 0.4));
    let reply = BlockBuilder::new(
        key(),
        policy(),
        vec![lane(SearchLaneKind::Semantic, one_then_one)],
    )
    .unwrap()
    .build_for_request(PageRequest {
        offset: 0,
        top_k: fixture.n_eff.top_k,
    })
    .unwrap();
    assert_eq!(
        reply.effective_target,
        fixture.n_eff.expected_effective_target
    );
    assert_eq!(reply.canonical_list.len(), 3);
    assert_eq!(reply.retrieval_depth, 400);
    assert_eq!(reply.page.len(), 1);

    let two_at_start = BlockBuilder::new(
        key(),
        policy(),
        vec![lane(
            SearchLaneKind::Semantic,
            vec![
                non_exact("first.rs", 1.0),
                non_exact("second.rs", 0.9),
                non_exact("third.rs", 0.8),
            ],
        )],
    )
    .unwrap()
    .build_for_request(PageRequest {
        offset: 0,
        top_k: fixture.n_eff.top_k,
    })
    .unwrap();
    assert_eq!(two_at_start.retrieval_depth, 200);
    assert_eq!(two_at_start.canonical_list.len(), 3);
}

#[test]
fn exact_only_is_depth_exempt_score_free_and_tail_ordered() {
    let fixture = fixture();
    let exact = (0..fixture.exact_only_count)
        .rev()
        .map(|position| {
            LaneCandidate::exact(
                format!("exact-{position:03}.rs"),
                None,
                EvidenceDescriptor::for_e1(1, true, false),
                false,
            )
        })
        .collect::<Vec<_>>();
    let exact_lane = lane(SearchLaneKind::Exact, exact);
    let empty_lexical = lane(SearchLaneKind::Lexical, Vec::new());
    let empty_semantic = lane(SearchLaneKind::Semantic, Vec::new());
    let shallow = BlockBuilder::new(
        key(),
        ScoringPolicy::empty(),
        vec![
            exact_lane.clone(),
            empty_lexical.clone(),
            empty_semantic.clone(),
        ],
    )
    .unwrap()
    .build_at_depth(200)
    .unwrap();
    let shallow_reversed = BlockBuilder::new(
        key(),
        ScoringPolicy::empty(),
        vec![empty_semantic, empty_lexical, exact_lane.clone()],
    )
    .unwrap()
    .build_at_depth(200)
    .unwrap();
    let deep = BlockBuilder::new(key(), ScoringPolicy::empty(), vec![exact_lane])
        .unwrap()
        .build_at_depth(3_200)
        .unwrap();

    let b0 = shallow.canonical_list.block(0).unwrap();
    assert_eq!(b0.entries.len(), fixture.exact_only_count);
    for (index, entry) in b0.entries.iter().enumerate() {
        assert_eq!(
            entry.result.path,
            PathBuf::from(format!("exact-{index:03}.rs"))
        );
        assert_eq!(entry.tier_index, 0);
        assert!(entry.admitted_contributions.is_empty());
        assert_eq!(entry.result.fusion_score, None);
        assert_eq!(entry.result.lane_score, None);
        let encoded = serde_json::to_value(&entry.result).unwrap();
        assert!(encoded.get("fusion_score").is_none());
        assert!(encoded.get("lane_score").is_none());
        assert_eq!(entry.lane_attribution.len(), 1);
        assert_eq!(
            entry.lane_attribution[0].disposition,
            ContributionDisposition::DepthExempt
        );
    }
    assert_eq!(
        serde_json::to_vec(&shallow).unwrap(),
        serde_json::to_vec(&shallow_reversed).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&shallow.canonical_list.stability_units()).unwrap(),
        serde_json::to_vec(&deep.canonical_list.stability_units()).unwrap()
    );
}

#[test]
fn mixed_exact_hits_precede_every_non_exact_hit_at_every_depth() {
    let fixture = fixture();
    let exact = (0..fixture.mixed_exact_count)
        .map(|position| {
            LaneCandidate::exact(
                format!("z-exact-{position:03}.rs"),
                None,
                EvidenceDescriptor::for_e1(1, true, false),
                false,
            )
        })
        .collect::<Vec<_>>();
    let non_exact = (0..500)
        .map(|position| non_exact(format!("a-non-exact-{position:03}.rs"), 100.0))
        .collect::<Vec<_>>();

    for depth in fixture.depths {
        let semantic_lane = lane(SearchLaneKind::Semantic, non_exact.clone());
        let exact_lane = lane(SearchLaneKind::Exact, exact.clone());
        let reply = BlockBuilder::new(
            key(),
            policy(),
            vec![semantic_lane.clone(), exact_lane.clone()],
        )
        .unwrap()
        .build_at_depth(depth)
        .unwrap();
        let reverse_reply = BlockBuilder::new(key(), policy(), vec![exact_lane, semantic_lane])
            .unwrap()
            .build_at_depth(depth)
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&reply).unwrap(),
            serde_json::to_vec(&reverse_reply).unwrap()
        );

        let entries = reply.canonical_list.entries().collect::<Vec<_>>();
        assert!(entries[..fixture.mixed_exact_count]
            .iter()
            .all(|entry| entry.result.evidence.tier == evidence_descriptor::EvidenceTier::Exact));
        assert!(entries[..fixture.mixed_exact_count]
            .iter()
            .all(|entry| entry.result.fusion_score.is_none() && entry.result.lane_score.is_none()));
        assert!(
            entries[fixture.mixed_exact_count..]
                .iter()
                .all(|entry| entry.result.evidence.tier
                    == evidence_descriptor::EvidenceTier::NonExact)
        );
    }
}

#[test]
fn list_identity_and_dedup_do_not_depend_on_paging_arguments() {
    let candidate = non_exact("deduplicated.rs", 1.0);
    let semantic = lane(SearchLaneKind::Semantic, vec![candidate.clone()]);
    let lexical = lane(SearchLaneKind::Lexical, vec![candidate]);
    let builder = BlockBuilder::new(key(), policy(), vec![semantic, lexical]).unwrap();
    let first = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 1,
        })
        .unwrap();
    let direct = builder
        .build_for_request(PageRequest {
            offset: 1,
            top_k: 1,
        })
        .unwrap();
    assert_eq!(first.canonical_list.key, direct.canonical_list.key);
    assert_eq!(first.canonical_list.len(), 1);
    assert!(!first.canonical_list.is_empty());
    assert_eq!(direct.canonical_list.len(), 1);
    assert_eq!(
        first.canonical_list.stability_units(),
        direct.canonical_list.stability_units()
    );
    assert!(direct.page.is_empty());

    let _candidate_shape_proof: CandidateResult = first.page[0].result.clone();
}
