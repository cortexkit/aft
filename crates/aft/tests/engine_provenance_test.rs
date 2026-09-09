use std::path::PathBuf;

mod comparator {
    pub use aft::commands::semantic_search::comparator::*;
}
mod evidence_descriptor {
    pub use aft::commands::semantic_search::evidence_descriptor::*;
}
mod plan_table {
    pub use aft::commands::semantic_search::plan_table::*;
}

#[allow(dead_code)]
#[path = "../src/commands/semantic_search/blocks.rs"]
mod blocks;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/provenance.rs"]
mod provenance;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/scoring.rs"]
mod scoring;

use blocks::{
    BlockBuilder, CanonicalLane, CanonicalListKey, ContributionDisposition, LaneAttribution,
    LaneCandidate, PageRequest, BLOCK_DEPTHS,
};
use evidence_descriptor::EvidenceDescriptor;
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use provenance::{
    LanePosition, LanePositionsAccessor, ObservedProvenance, ProvenanceError,
    SpecialLaneDisposition,
};
use scoring::ScoringPolicy;

fn policy() -> ScoringPolicy {
    ScoringPolicy::from_plan_table(&PlanTable::running_table(), SearchShape::NaturalLanguage)
        .expect("fixture scoring policy")
}

fn non_exact(path: impl Into<PathBuf>, score: f32) -> LaneCandidate {
    LaneCandidate::non_exact(
        path,
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        score,
        false,
    )
}

fn r18_builder() -> (BlockBuilder, PathBuf) {
    let target = PathBuf::from("target.rs");
    let lexical = (0..260)
        .map(|position| {
            if position == 250 {
                non_exact(target.clone(), 1.0)
            } else {
                non_exact(format!("lexical-{position:03}.rs"), 1.0)
            }
        })
        .collect();
    let semantic = (0..11)
        .map(|position| {
            if position == 10 {
                non_exact(target.clone(), 1.0)
            } else {
                non_exact(format!("semantic-{position:03}.rs"), 1.0)
            }
        })
        .collect();
    let lanes = vec![
        CanonicalLane::new(SearchLaneKind::Lexical, lexical).expect("lexical lane"),
        CanonicalLane::new(SearchLaneKind::Semantic, semantic).expect("semantic lane"),
    ];
    let builder = BlockBuilder::new(
        CanonicalListKey {
            project_root: PathBuf::from("/fixture"),
            snapshot_generation: "generation-token".to_string(),
            normalized_query: "target".to_string(),
            include_tests: false,
        },
        policy(),
        lanes,
    )
    .expect("block builder");
    (builder, target)
}

#[test]
fn unobserved_is_absent_then_observed_unadmitted_exactly_at_candidate_tier_cutoff() {
    let (builder, target) = r18_builder();
    let shallow = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 10,
        })
        .expect("tier-zero page");
    assert_eq!(shallow.retrieval_depth, BLOCK_DEPTHS[0]);
    assert_eq!(
        shallow
            .lane_enumeration_counts
            .get(&SearchLaneKind::Lexical),
        Some(&BLOCK_DEPTHS[0]),
        "tier-zero telemetry must report exactly D_0 lexical candidates"
    );

    let shallow_entry = shallow
        .canonical_list
        .entries()
        .find(|entry| entry.result.path == target)
        .expect("target in B_0");
    let shallow_positions = ObservedProvenance::from_reply(&shallow)
        .expect("shallow provenance")
        .lane_positions(shallow_entry)
        .expect("shallow positions");
    assert!(!shallow_positions.contains_key(&SearchLaneKind::Lexical));
    assert_eq!(
        shallow_positions.get(&SearchLaneKind::Semantic),
        Some(&LanePosition::DepthLimited {
            position: 10,
            admitted: true,
        })
    );

    let deep = builder.build_at_depth(400).expect("depth-400 reply");
    let deep_entry = deep
        .canonical_list
        .entries()
        .find(|entry| entry.result.path == target)
        .expect("target in reconstructed B_0");
    let deep_positions = ObservedProvenance::from_reply(&deep)
        .expect("deep provenance")
        .lane_positions(deep_entry)
        .expect("deep positions");
    assert_eq!(
        deep_positions.get(&SearchLaneKind::Lexical),
        Some(&LanePosition::DepthLimited {
            position: 250,
            admitted: false,
        })
    );
    assert_eq!(deep_entry.tier_index, 0);
    assert!(250 >= BLOCK_DEPTHS[deep_entry.tier_index]);
    assert_eq!(shallow_entry.stability_unit(), deep_entry.stability_unit());
}

#[test]
fn exact_is_depth_exempt_and_observed_depth_limited_hits_are_provenance_only() {
    let path = PathBuf::from("exact.rs");
    let lanes = vec![
        CanonicalLane::new(
            SearchLaneKind::Exact,
            vec![LaneCandidate::exact(
                path.clone(),
                None,
                EvidenceDescriptor::for_e1(2, true, false),
                false,
            )],
        )
        .expect("exact lane"),
        CanonicalLane::new(SearchLaneKind::Lexical, vec![non_exact(path.clone(), 4.0)])
            .expect("lexical lane"),
    ];
    let builder = BlockBuilder::new(
        CanonicalListKey {
            project_root: PathBuf::from("/fixture"),
            snapshot_generation: "generation-token".to_string(),
            normalized_query: "exact".to_string(),
            include_tests: false,
        },
        policy(),
        lanes,
    )
    .expect("builder");
    let reply = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 1,
        })
        .expect("page");
    let entry = reply.page.first().expect("exact result");
    let positions = ObservedProvenance::from_reply(&reply)
        .expect("provenance")
        .lane_positions(entry)
        .expect("positions");

    assert_eq!(
        positions.get(&SearchLaneKind::Exact),
        Some(&LanePosition::Special {
            position: 0,
            disposition: SpecialLaneDisposition::DepthExempt,
        })
    );
    assert_eq!(
        positions.get(&SearchLaneKind::Lexical),
        Some(&LanePosition::Special {
            position: 0,
            disposition: SpecialLaneDisposition::ProvenanceOnly,
        })
    );
    let encoded = serde_json::to_value(&positions).expect("serialize positions");
    assert!(encoded["exact"].get("admitted").is_none());
    assert!(encoded["lexical"].get("admitted").is_none());
    assert!(entry.result.fusion_score.is_none());
    assert!(entry.result.lane_score.is_none());
}

#[test]
fn full_canonical_order_mutation_red_rejects_a_position_past_the_observed_prefix() {
    let (builder, target) = r18_builder();
    let shallow = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 10,
        })
        .expect("tier-zero page");
    let mut mutated = shallow
        .canonical_list
        .entries()
        .find(|entry| entry.result.path == target)
        .expect("target")
        .clone();
    mutated.lane_attribution.push(LaneAttribution {
        lane: SearchLaneKind::Lexical,
        position: 250,
        disposition: ContributionDisposition::NotAdmitted,
    });

    let error = ObservedProvenance::from_reply(&shallow)
        .expect("provenance")
        .lane_positions(&mutated)
        .expect_err("full canonical order must not populate shallow provenance");
    assert_eq!(
        error,
        ProvenanceError::UnobservedPosition {
            lane: SearchLaneKind::Lexical,
            position: 250,
            observed_count: BLOCK_DEPTHS[0],
        }
    );
}

#[test]
fn probing_past_d0_for_telemetry_mutation_red_is_rejected() {
    let (builder, _) = r18_builder();
    let mut shallow = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 10,
        })
        .expect("tier-zero page");
    shallow
        .lane_enumeration_counts
        .insert(SearchLaneKind::Lexical, BLOCK_DEPTHS[0] + 1);

    assert_eq!(
        ObservedProvenance::from_reply(&shallow)
            .err()
            .expect("a tier-zero run cannot probe beyond D_0"),
        ProvenanceError::EnumerationPastReachedDepth {
            lane: SearchLaneKind::Lexical,
            observed_count: BLOCK_DEPTHS[0] + 1,
            retrieval_depth: BLOCK_DEPTHS[0],
        }
    );
}

#[test]
fn no_marker_or_placeholder_is_serialized_for_an_unobserved_lane() {
    let (builder, target) = r18_builder();
    let reply = builder
        .build_for_request(PageRequest {
            offset: 0,
            top_k: 10,
        })
        .expect("tier-zero page");
    let entry = reply
        .canonical_list
        .entries()
        .find(|entry| entry.result.path == target)
        .expect("target");
    let positions = ObservedProvenance::from_reply(&reply)
        .expect("provenance")
        .lane_positions(entry)
        .expect("positions");
    let object = serde_json::to_value(positions)
        .expect("serialize")
        .as_object()
        .expect("lane positions object")
        .clone();

    assert_eq!(object.len(), 1);
    assert!(object.contains_key("semantic"));
    assert!(!object.contains_key("lexical"));
    assert!(object.values().all(|value| !value.is_null()));
    assert!(!object.values().any(|value| value == "not_observed"));
}

#[test]
fn observed_unadmitted_wire_record_is_position_and_false_only() {
    let (builder, target) = r18_builder();
    let reply = builder.build_at_depth(400).expect("depth-400 reply");
    let entry = reply
        .canonical_list
        .entries()
        .find(|entry| entry.result.path == target)
        .expect("target");
    let positions = ObservedProvenance::from_reply(&reply)
        .expect("provenance")
        .lane_positions(entry)
        .expect("positions");
    assert_eq!(
        serde_json::to_value(&positions[&SearchLaneKind::Lexical]).expect("serialize"),
        serde_json::json!({"position": 250, "admitted": false})
    );
}
