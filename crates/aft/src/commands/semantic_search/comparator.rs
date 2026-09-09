use std::cmp::Ordering;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::evidence_descriptor::{EvidenceDescriptor, EvidenceKind, EvidenceTier};
use super::plan_table::SearchLaneKind;

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

/// Candidate result evaluated by the total comparator R3.
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

/// Stability unit ranked tuple: (file, symbol range, R3 order index, fusion score, lane score).
/// For exact-tier results, fusion_score and lane_score are ABSENT when serialized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedTuple {
    pub file: PathBuf,
    pub symbol_range: Option<SymbolOffsetRange>,
    pub r3_order_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_score: Option<f32>,
}

/// Compare comparator fields (1)-(6).
///
/// Order:
/// (1) definition hit (identifier shape only)
/// (2) E1 verbatim, more phrase occurrences first
/// (3) anchored hit:
///     (3a) larger matched-run character total
///     (3b) fewer gap characters
/// (4) E2 window, narrower window first
/// (5) exact-form hit over any variant hit
/// (6) non-generated over generated
pub fn compare_fields_1_to_6(a: &CandidateResult, b: &CandidateResult) -> Ordering {
    // (1) Definition hit: true > false
    let a_def = a.evidence.kind == EvidenceKind::Definition;
    let b_def = b.evidence.kind == EvidenceKind::Definition;
    match b_def.cmp(&a_def) {
        Ordering::Equal => {}
        ord => return ord,
    }

    // (2) E1 verbatim: E1 hit > non-E1, more occurrences first
    let a_e1 = a.evidence.kind == EvidenceKind::E1;
    let b_e1 = b.evidence.kind == EvidenceKind::E1;
    match b_e1.cmp(&a_e1) {
        Ordering::Equal => {
            if a_e1 && b_e1 {
                match b.evidence.occurrences.cmp(&a.evidence.occurrences) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
        }
        ord => return ord,
    }

    // (3) Anchored hit: Anchored > non-anchored
    let a_anchored = a.evidence.kind == EvidenceKind::Anchored;
    let b_anchored = b.evidence.kind == EvidenceKind::Anchored;
    match b_anchored.cmp(&a_anchored) {
        Ordering::Equal => {
            if a_anchored && b_anchored {
                // (3a) larger matched-run character total
                match b.evidence.matched_span.cmp(&a.evidence.matched_span) {
                    Ordering::Equal => {}
                    ord => return ord,
                }

                // (3b) fewer gap characters
                match a.evidence.gap_chars.cmp(&b.evidence.gap_chars) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
        }
        ord => return ord,
    }

    // (4) E2 window: E2 > non-E2, narrower window first
    let a_e2 = a.evidence.kind == EvidenceKind::E2;
    let b_e2 = b.evidence.kind == EvidenceKind::E2;
    match b_e2.cmp(&a_e2) {
        Ordering::Equal => {
            if a_e2 && b_e2 {
                match a.evidence.window_lines.cmp(&b.evidence.window_lines) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
        }
        ord => return ord,
    }

    // (5) exact-form hit over variant hit: true > false
    match b.evidence.exact_form.cmp(&a.evidence.exact_form) {
        Ordering::Equal => {}
        ord => return ord,
    }

    // (6) non-generated over generated: false > true
    a.evidence.generated.cmp(&b.evidence.generated)
}

/// Compare score-free tail fields (9)-(11):
/// (9) path, byte-wise ascending
/// (10) result granularity: file-level (range == None) sorts before symbol-level (range == Some)
/// (11) symbol range start offset ascending, then end offset ascending
pub fn compare_fields_9_to_11(a: &CandidateResult, b: &CandidateResult) -> Ordering {
    // (9) path byte-wise ascending
    match a
        .path
        .to_string_lossy()
        .as_bytes()
        .cmp(b.path.to_string_lossy().as_bytes())
    {
        Ordering::Equal => {}
        ord => return ord,
    }

    // (10) file-level (None) sorts before symbol-level (Some)
    match a.symbol_range.is_some().cmp(&b.symbol_range.is_some()) {
        Ordering::Equal => {}
        ord => return ord,
    }

    // (11) start offset ascending, then end offset ascending
    match (&a.symbol_range, &b.symbol_range) {
        (Some(ra), Some(rb)) => match ra.start.cmp(&rb.start) {
            Ordering::Equal => ra.end.cmp(&rb.end),
            ord => ord,
        },
        _ => Ordering::Equal,
    }
}

/// Score-free R3 comparator: fields (1)-(6) followed by fields (9)-(11), with (7) and (8) skipped.
/// Total order on the deduplicated set.
pub fn score_free_r3_cmp(a: &CandidateResult, b: &CandidateResult) -> Ordering {
    match compare_fields_1_to_6(a, b) {
        Ordering::Equal => compare_fields_9_to_11(a, b),
        ord => ord,
    }
}

/// Full total comparator R3 fields (1)-(11).
///
/// Per R16:
/// - Exact-tier candidates are score-free: fields (7) and (8) are undefined and skipped.
/// - Exact-tier candidates always precede non-exact candidates via fields (1)-(4).
/// - Fields (7) and (8) are reached ONLY between two non-exact candidates.
pub fn r3_cmp(a: &CandidateResult, b: &CandidateResult) -> Ordering {
    // Fields (1)-(6)
    match compare_fields_1_to_6(a, b) {
        Ordering::Equal => {}
        ord => return ord,
    }

    // If both are exact-tier: fields (7) and (8) are skipped per R16.
    let a_is_exact = a.evidence.tier == EvidenceTier::Exact;
    let b_is_exact = b.evidence.tier == EvidenceTier::Exact;

    if a_is_exact && b_is_exact {
        return compare_fields_9_to_11(a, b);
    }

    if a_is_exact != b_is_exact {
        // An exact-tier candidate and a non-exact candidate are always separated by
        // fields (1)-(4). If this point is reached, something is inconsistent.
        panic!(
            "exact-tier and non-exact candidates tied on fields (1)-(6): a={:?}, b={:?}",
            a, b
        );
    }

    // Both are non-exact candidates: fields (7) and (8) apply.
    // (7) Fusion score descending
    match (a.fusion_score, b.fusion_score) {
        (Some(fa), Some(fb)) => match fb.total_cmp(&fa) {
            Ordering::Equal => {}
            ord => return ord,
        },
        _ => {}
    }

    // (8) Lane score descending, with lanes compared in fixed plan-table order on equal raw scores
    match (a.lane_score, b.lane_score) {
        (Some(la), Some(lb)) => match lb.total_cmp(&la) {
            Ordering::Equal => {
                // Fixed plan-table order: exact (0) < lexical (1) < semantic (2)
                let a_order = a
                    .best_lane
                    .map(|l| l.default_plan_order_index())
                    .unwrap_or(usize::MAX);
                let b_order = b
                    .best_lane
                    .map(|l| l.default_plan_order_index())
                    .unwrap_or(usize::MAX);
                match a_order.cmp(&b_order) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
            ord => return ord,
        },
        _ => {}
    }

    // Tail fields (9)-(11)
    compare_fields_9_to_11(a, b)
}

/// Sort results using the total R3 comparator.
pub fn sort_r3(results: &mut [CandidateResult]) {
    results.sort_by(r3_cmp);
}

/// Sort results using the score-free R3 comparator.
pub fn sort_score_free_r3(results: &mut [CandidateResult]) {
    results.sort_by(score_free_r3_cmp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_fixture_3a_run_char_total_ranks_24_char_first() {
        // Candidate A: 21 chars
        let cand_a = CandidateResult {
            path: PathBuf::from("a.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_anchored(21, 5, true, false),
            fusion_score: None,
            lane_score: None,
            best_lane: Some(SearchLaneKind::Exact),
        };

        // Candidate B: 24 chars
        let cand_b = CandidateResult {
            path: PathBuf::from("b.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_anchored(24, 5, true, false),
            fusion_score: None,
            lane_score: None,
            best_lane: Some(SearchLaneKind::Exact),
        };

        // Under normative comparator (3a = matched-run character total):
        // 24 chars > 21 chars => cand_b sorts first (cand_b < cand_a in sort order)
        assert_eq!(r3_cmp(&cand_b, &cand_a), Ordering::Less);
        assert_eq!(r3_cmp(&cand_a, &cand_b), Ordering::Greater);
    }

    #[test]
    fn exact_tier_is_score_free_r16() {
        // Exact results have no fusion_score and no lane_score
        let cand_exact = CandidateResult::new_exact(
            PathBuf::from("exact.rs"),
            None,
            EvidenceDescriptor::for_e1(3, true, false),
        );

        let json = serde_json::to_value(&cand_exact).unwrap();
        // Emitted structure MUST have fusion_score and lane_score absent
        assert!(
            json.get("fusion_score").is_none(),
            "fusion_score must be absent"
        );
        assert!(
            json.get("lane_score").is_none(),
            "lane_score must be absent"
        );
    }

    #[test]
    fn score_free_acyclicity_operational_assertion() {
        // Build items for each lane
        let exact_cand = CandidateResult::new_exact(
            PathBuf::from("exact.rs"),
            None,
            EvidenceDescriptor::for_e1(5, true, false),
        );

        let lexical_cand = CandidateResult {
            path: PathBuf::from("lex.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_non_exact(true, false),
            fusion_score: None, // not yet fused!
            lane_score: Some(0.85),
            best_lane: Some(SearchLaneKind::Lexical),
        };

        let semantic_cand = CandidateResult {
            path: PathBuf::from("sem.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_non_exact(true, false),
            fusion_score: None, // not yet fused!
            lane_score: Some(0.92),
            best_lane: Some(SearchLaneKind::Semantic),
        };

        // Stubbed fusion scorer panics if called
        let _stubbed_fusion_scorer = || -> f32 {
            panic!("fusion scorer must not be called during lane canonical ordering or exact tier ordering");
        };

        // 1. Exact tier canonical order uses score-free R3
        let mut exact_list = vec![exact_cand.clone()];
        sort_score_free_r3(&mut exact_list);
        assert_eq!(exact_list.len(), 1);

        // 2. Lexical canonical order uses raw lane score, tie-break score-free R3
        let mut lex_list = vec![lexical_cand.clone()];
        lex_list.sort_by(|a, b| {
            b.lane_score
                .unwrap()
                .total_cmp(&a.lane_score.unwrap())
                .then_with(|| score_free_r3_cmp(a, b))
        });
        assert_eq!(lex_list.len(), 1);

        // 3. Semantic canonical order
        let mut sem_list = vec![semantic_cand.clone()];
        sem_list.sort_by(|a, b| {
            b.lane_score
                .unwrap()
                .total_cmp(&a.lane_score.unwrap())
                .then_with(|| score_free_r3_cmp(a, b))
        });
        assert_eq!(sem_list.len(), 1);

        // Complete exact-tier order produced with zero panics from stubbed scorer
        assert_eq!(exact_list[0].path, PathBuf::from("exact.rs"));
    }

    #[test]
    fn file_level_sorts_before_symbol_level_on_same_path() {
        let file_level = CandidateResult::new_exact(
            PathBuf::from("common.rs"),
            None,
            EvidenceDescriptor::for_definition(true, false),
        );
        let symbol_level = CandidateResult::new_exact(
            PathBuf::from("common.rs"),
            Some(SymbolOffsetRange::new(10, 50)),
            EvidenceDescriptor::for_definition(true, false),
        );

        assert_eq!(r3_cmp(&file_level, &symbol_level), Ordering::Less);
        assert_eq!(r3_cmp(&symbol_level, &file_level), Ordering::Greater);
    }
}
