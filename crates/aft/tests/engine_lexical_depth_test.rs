use std::path::{Path, PathBuf};

use aft::commands::semantic_search::comparator::CandidateResult;
use aft::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;
use aft::search_index::SearchIndex;
use serde::{Deserialize, Serialize};

use aft::commands::semantic_search::lexical_lane;

use lexical_lane::{
    CanonicalLexicalLane, LanePosition, LexicalCandidate, LEXICAL_DEPTHS,
    LEXICAL_ENUMERATION_LIMIT, LEXICAL_MAX_DEPTH,
};

#[derive(Debug, Deserialize)]
struct DepthFixture {
    query_token: String,
    query_trigram_count: usize,
    pool_size: usize,
    target_position: usize,
    shallow_depth: usize,
    deep_depth: usize,
    initial_batch_sizes: [usize; 2],
    equal_score_pool_size: usize,
    late_high_score_discovery_position: usize,
}

fn fixture() -> DepthFixture {
    serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/lexical/depth-prefix.json"
    ))
    .expect("lexical depth fixture must be valid JSON")
}

fn equal_score_live_lane(
    fixture: &DepthFixture,
    initial_batch_size: usize,
) -> (CanonicalLexicalLane, PathBuf) {
    let mut index = SearchIndex::new();
    let root = PathBuf::from("/virtual/lexical-depth-fixture");
    let target = root.join(format!("rank_{:03}_target.rs", fixture.target_position));

    for rank in 0..fixture.pool_size {
        if rank == fixture.target_position {
            continue;
        }
        let path = root.join(format!("rank_{rank:03}_decoy.rs"));
        index.index_file(&path, fixture.query_token.as_bytes());
    }
    // Index the target after 200 decoys to verify that discovery does not stop at 200 files.
    index.index_file(&target, fixture.query_token.as_bytes());

    let trigrams = SearchIndex::query_trigrams_from_tokens(&[fixture.query_token.as_str()]);
    assert_eq!(trigrams.len(), fixture.query_trigram_count);
    let lane =
        CanonicalLexicalLane::from_snapshot(&index.snapshot(), &trigrams, None, initial_batch_size)
            .expect("build live lexical lane");
    (lane, target)
}

fn candidate(path: impl Into<PathBuf>, score: f32) -> LexicalCandidate {
    LexicalCandidate::file(path.into(), score)
}

#[test]
fn lexical_depth_constants_keep_fifty_as_batch_only() {
    assert_eq!(LEXICAL_ENUMERATION_LIMIT, 50);
    assert_eq!(LEXICAL_DEPTHS, [200, 400, 800, 1_600, 3_200]);
    assert_eq!(LEXICAL_MAX_DEPTH, 3_200);
}

#[test]
fn live_lane_exposes_position_250_beyond_the_old_pre_scoring_cap() {
    let fixture = fixture();
    let (mut lane, target) = equal_score_live_lane(&fixture, LEXICAL_ENUMERATION_LIMIT);
    assert_eq!(lane.selected_pool_size(), fixture.pool_size);

    let shallow = lane
        .enumerate_to_depth(fixture.shallow_depth)
        .expect("enumerate tier zero");
    assert_eq!(shallow.enumerated_count, fixture.shallow_depth);
    assert!(
        !shallow
            .hits
            .iter()
            .any(|hit| hit.candidate.result.path == target),
        "position-250 target must not leak into the top-200 prefix"
    );
    assert_eq!(lane.observed_lane_position(&target, 1), None);

    let deep = lane
        .enumerate_to_depth(fixture.deep_depth)
        .expect("enumerate tier one");
    assert_eq!(deep.enumerated_count, fixture.deep_depth);
    assert_eq!(
        deep.hits[fixture.target_position].candidate.result.path,
        target
    );
    assert_eq!(
        lane.observed_lane_position(&target, 1),
        Some(LanePosition {
            position: fixture.target_position,
            admitted: true,
        })
    );
    assert_eq!(
        &deep.hits[..fixture.shallow_depth],
        shallow.hits.as_slice(),
        "the canonical top-200 must be an exact prefix of the top-400"
    );
}

#[test]
fn high_score_after_file_id_200_ranks_second_before_batching() {
    let fixture = fixture();
    let mut discovered = Vec::new();
    discovered.push(candidate("z-first.rs", 1_000.0));
    for index in 1..fixture.late_high_score_discovery_position {
        discovered.push(candidate(format!("filler-{index:03}.rs"), 1.0));
    }
    let high = PathBuf::from("late-H.rs");
    discovered.push(candidate(high.clone(), 900.0));
    while discovered.len() < 260 {
        let index = discovered.len();
        discovered.push(candidate(format!("tail-{index:03}.rs"), 0.5));
    }

    assert_eq!(
        discovered
            .iter()
            .position(|entry| entry.result.path == high),
        Some(fixture.late_high_score_discovery_position)
    );
    let mut lane =
        CanonicalLexicalLane::from_scored_candidates(discovered, LEXICAL_ENUMERATION_LIMIT)
            .expect("build canonical lane");
    let shallow = lane.enumerate_to_depth(200).expect("enumerate top 200");
    assert_eq!(shallow.hits[1].candidate.result.path, high);
    assert!(shallow
        .hits
        .iter()
        .any(|hit| hit.candidate.result.path == high));

    let shallow_units = serde_json::to_vec(&shallow.hits).expect("serialize shallow units");
    let deep = lane
        .enumerate_to_depth(400)
        .expect("enumerate remaining pool");
    let deep_b0_units = serde_json::to_vec(&deep.hits[..200]).expect("serialize deep B0 units");
    assert_eq!(shallow_units, deep_b0_units);
}

#[test]
fn equal_score_last_discovered_candidate_wins_score_free_r3_tie_break() {
    let fixture = fixture();
    let mut discovered = (0..fixture.equal_score_pool_size - 1)
        .map(|index| candidate(format!("zzz-{index:03}.rs"), 7.0))
        .collect::<Vec<_>>();
    let equal_score_head = PathBuf::from("000-E.rs");
    discovered.push(candidate(equal_score_head.clone(), 7.0));
    assert_eq!(
        discovered
            .iter()
            .position(|entry| entry.result.path == equal_score_head),
        Some(fixture.equal_score_pool_size - 1)
    );

    let mut lane = CanonicalLexicalLane::from_scored_candidates(discovered, 50)
        .expect("build equal-score lane");
    let shallow = lane.enumerate_to_depth(200).expect("enumerate top 200");
    assert_eq!(shallow.hits[0].candidate.result.path, equal_score_head);

    let shallow_units = serde_json::to_vec(&shallow.hits).expect("serialize shallow units");
    let deep = lane.enumerate_to_depth(400).expect("enumerate full pool");
    let deep_b0_units = serde_json::to_vec(&deep.hits[..200]).expect("serialize deep B0 units");
    assert_eq!(shallow_units, deep_b0_units);
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct R18StabilityUnit {
    file: PathBuf,
    r3_order_index: usize,
    fusion_score: f32,
    lane_score: f32,
    admitted_lanes: Vec<&'static str>,
    evidence_descriptor: EvidenceDescriptor,
}

fn semantic_only_unit(path: &Path, candidate: &CandidateResult) -> R18StabilityUnit {
    let semantic_position = 10usize;
    let semantic_score = 1.0 / (60.0 + semantic_position as f32 + 1.0);
    R18StabilityUnit {
        file: path.to_path_buf(),
        r3_order_index: 0,
        fusion_score: semantic_score,
        lane_score: semantic_score,
        admitted_lanes: vec!["semantic"],
        evidence_descriptor: candidate.evidence.clone(),
    }
}

#[test]
fn r18_unobserved_then_observed_unadmitted_keeps_b0_stable() {
    let fixture = fixture();
    let (mut shallow_lane, target) = equal_score_live_lane(&fixture, 50);
    let shallow = shallow_lane
        .enumerate_to_depth(fixture.shallow_depth)
        .expect("enumerate shallow lane");
    assert_eq!(shallow_lane.enumeration_count(), fixture.shallow_depth);
    assert_eq!(shallow_lane.observed_lane_position(&target, 0), None);

    let (mut deep_lane, deep_target) = equal_score_live_lane(&fixture, 50);
    let deep = deep_lane
        .enumerate_to_depth(fixture.deep_depth)
        .expect("enumerate deep lane");
    assert_eq!(target, deep_target);
    assert_eq!(
        deep_lane.observed_lane_position(&target, 0),
        Some(LanePosition {
            position: fixture.target_position,
            admitted: false,
        })
    );

    let target_candidate = &deep.hits[fixture.target_position].candidate.result;
    let shallow_unit = semantic_only_unit(&target, target_candidate);
    let deep_unit = semantic_only_unit(&target, target_candidate);
    assert_eq!(
        serde_json::to_vec(&shallow_unit).expect("serialize shallow stability unit"),
        serde_json::to_vec(&deep_unit).expect("serialize deep stability unit")
    );
    assert_eq!(shallow_unit.admitted_lanes, vec!["semantic"]);
    assert_eq!(deep_unit.admitted_lanes, vec!["semantic"]);

    assert_eq!(shallow.retrieval_depth, 200);
    assert_eq!(shallow.depth_tier, 0);
    assert!(!shallow.lanes_exhausted);
    assert_eq!(deep.retrieval_depth, 400);
    assert_eq!(deep.depth_tier, 1);
    assert!(!deep.lanes_exhausted);

    let shallow_b0 = serde_json::to_vec(&shallow.hits).expect("serialize shallow B0");
    let deep_b0 =
        serde_json::to_vec(&deep.hits[..fixture.shallow_depth]).expect("serialize deep B0");
    assert_eq!(shallow_b0, deep_b0);
}

#[test]
fn initial_batch_size_does_not_change_order_or_reply_at_reached_depth() {
    let fixture = fixture();
    let (mut lane_50, target_50) = equal_score_live_lane(&fixture, fixture.initial_batch_sizes[0]);
    let (mut lane_400, target_400) =
        equal_score_live_lane(&fixture, fixture.initial_batch_sizes[1]);
    assert_eq!(target_50, target_400);

    let reply_50 = lane_50
        .enumerate_to_depth(fixture.deep_depth)
        .expect("enumerate in 50-file batches");
    let reply_400 = lane_400
        .enumerate_to_depth(fixture.deep_depth)
        .expect("enumerate in one 400-file batch");

    assert_eq!(lane_50.canonical_order(), lane_400.canonical_order());
    assert_eq!(
        serde_json::to_vec(&reply_50).expect("serialize batch-50 reply"),
        serde_json::to_vec(&reply_400).expect("serialize batch-400 reply")
    );
}

#[test]
fn tier_zero_cost_does_not_probe_deeper_for_provenance() {
    let fixture = fixture();
    let (mut lane, target) = equal_score_live_lane(&fixture, 50);
    let reply = lane
        .enumerate_to_depth(fixture.shallow_depth)
        .expect("enumerate tier zero");

    assert_eq!(reply.enumerated_count, fixture.shallow_depth);
    assert_eq!(lane.enumeration_count(), fixture.shallow_depth);
    assert_eq!(lane.observed_lane_position(&target, 0), None);
}
