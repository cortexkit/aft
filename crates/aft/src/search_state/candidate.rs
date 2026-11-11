use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::evidence_descriptor::{EvidenceDescriptor, EvidenceTier};
use crate::search_b2::plan_table::SearchLaneKind;

/// Byte offset range for a symbol result within a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolOffsetRange {
    pub start: usize,
    pub end: usize,
}

impl SymbolOffsetRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Candidate result evaluated by the search result comparator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateResult {
    pub path: PathBuf,
    pub symbol_range: Option<SymbolOffsetRange>,
    pub evidence: EvidenceDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_lane: Option<SearchLaneKind>,
}

impl CandidateResult {
    pub fn new_exact(
        path: PathBuf,
        symbol_range: Option<SymbolOffsetRange>,
        evidence: EvidenceDescriptor,
    ) -> Self {
        assert_eq!(
            evidence.tier,
            EvidenceTier::Exact,
            "exact candidate must carry EvidenceTier::Exact"
        );
        Self {
            path,
            symbol_range,
            evidence,
            fusion_score: None,
            lane_score: None,
            best_lane: Some(SearchLaneKind::Exact),
        }
    }

    pub fn new_non_exact(
        path: PathBuf,
        symbol_range: Option<SymbolOffsetRange>,
        evidence: EvidenceDescriptor,
        fusion_score: f32,
        lane_score: f32,
        best_lane: SearchLaneKind,
    ) -> Self {
        assert_eq!(
            evidence.tier,
            EvidenceTier::NonExact,
            "non-exact candidate must carry EvidenceTier::NonExact"
        );
        Self {
            path,
            symbol_range,
            evidence,
            fusion_score: Some(fusion_score),
            lane_score: Some(lane_score),
            best_lane: Some(best_lane),
        }
    }
}
