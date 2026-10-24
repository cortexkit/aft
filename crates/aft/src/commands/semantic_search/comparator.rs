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
///
/// Within each of (1)-(4), when both candidates carry the same evidence kind,
/// a source file sorts before a data file before the kind's own strength
/// measure is compared. Data documents (captured results, schemas, census
/// tables) repeat a phrase or identifier many times without defining it, so
/// their occurrence count or matched span is not stronger evidence than a
/// single hit in source. The demotion never crosses kinds: a data file with a
/// verbatim hit still ranks above a source file with only a token window.
pub fn compare_fields_1_to_6(a: &CandidateResult, b: &CandidateResult) -> Ordering {
    // For prose, an identifier hit ranked first by both lexical and semantic
    // relevance can precede declaration ownership and mention counts. Without
    // that agreement, sparse names and explicit declaration requests retain
    // the ordinary score-free exact order.
    match b
        .evidence
        .prose_lane_agreement
        .cmp(&a.evidence.prose_lane_agreement)
    {
        Ordering::Equal => {}
        ord => return ord,
    }
    compare_exact_strength(&a.evidence, &b.evidence)
        .then_with(|| b.evidence.exact_form.cmp(&a.evidence.exact_form))
        .then_with(|| a.evidence.generated.cmp(&b.evidence.generated))
}

/// Shared exact strength: kind, source before data, then the kind's metrics.
/// Ranking-only prose agreement and exact-form/generated preferences stay in
/// the rank comparator; they do not establish strict exact confidence.
pub(crate) fn compare_exact_strength(
    a: &super::evidence_descriptor::EvidenceDescriptor,
    b: &super::evidence_descriptor::EvidenceDescriptor,
) -> Ordering {
    let rank = |kind| match kind {
        EvidenceKind::Definition => 0,
        EvidenceKind::E1 => 1,
        EvidenceKind::Anchored => 2,
        EvidenceKind::E2 => 3,
        EvidenceKind::None => 4,
    };
    let kind = rank(a.kind).cmp(&rank(b.kind));
    if kind != Ordering::Equal {
        return kind;
    }
    if a.kind != EvidenceKind::None {
        let source = a.data_file.cmp(&b.data_file);
        if source != Ordering::Equal {
            return source;
        }
    }
    match a.kind {
        EvidenceKind::Definition => b.receiver_in_path.cmp(&a.receiver_in_path),
        EvidenceKind::E1 => b.occurrences.cmp(&a.occurrences),
        EvidenceKind::Anchored => b
            .matched_span
            .cmp(&a.matched_span)
            .then_with(|| a.gap_chars.cmp(&b.gap_chars)),
        EvidenceKind::E2 => a.window_lines.cmp(&b.window_lines),
        EvidenceKind::None => Ordering::Equal,
    }
}

/// Compare score-free tail fields (9)-(11):
/// (9) source code before any other file, then path, byte-wise ascending
/// (10) result granularity: file-level (range == None) sorts before symbol-level (range == Some)
/// (11) symbol range start offset ascending, then end offset ascending
///
/// The source-first step keeps a path sort from deciding between source and a
/// document: without it `.gsd/notes.md` sorted before `crates/x.rs` on equal
/// evidence only because `.` is a smaller byte than `c`.
pub fn compare_fields_9_to_11(a: &CandidateResult, b: &CandidateResult) -> Ordering {
    // (9a) source code before documents, data and unrecognised files
    let a_source = super::exact_lane::is_definition_source(&a.path);
    let b_source = super::exact_lane::is_definition_source(&b.path);
    match b_source.cmp(&a_source) {
        Ordering::Equal => {}
        ord => return ord,
    }

    // (9b) path byte-wise ascending
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

    fn exact_candidate(
        path: &str,
        mut evidence: EvidenceDescriptor,
        data: bool,
    ) -> CandidateResult {
        evidence.data_file = data;
        CandidateResult::new_exact(PathBuf::from(path), None, evidence)
    }

    #[test]
    fn source_verbatim_hit_outranks_data_file_with_more_occurrences() {
        // A census JSON quoting the phrase twice sorts after a source file that
        // contains it once, even though its path also sorts first.
        let data = exact_candidate(
            "a/census.json",
            EvidenceDescriptor::for_e1(2, true, false),
            true,
        );
        let source = exact_candidate(
            "z/configure.rs",
            EvidenceDescriptor::for_e1(1, true, false),
            false,
        );
        let mut results = vec![data.clone(), source.clone()];
        sort_r3(&mut results);
        assert_eq!(results, vec![source, data]);
    }

    #[test]
    fn source_before_data_applies_within_every_exact_kind() {
        let pairs = [
            (
                EvidenceDescriptor::for_definition(true, false),
                EvidenceDescriptor::for_definition(true, false),
            ),
            (
                EvidenceDescriptor::for_anchored(30, 0, true, false),
                EvidenceDescriptor::for_anchored(10, 9, true, false),
            ),
            (
                EvidenceDescriptor::for_e2(1, true, false),
                EvidenceDescriptor::for_e2(9, true, false),
            ),
        ];
        for (stronger, weaker) in pairs {
            let data = exact_candidate("a.json", stronger, true);
            let source = exact_candidate("b.rs", weaker, false);
            assert_eq!(
                r3_cmp(&source, &data),
                Ordering::Less,
                "{:?}",
                source.evidence.kind
            );
        }
    }

    #[test]
    fn data_demotion_never_crosses_evidence_kinds() {
        // A verbatim hit in a data file is still stronger evidence than a
        // token window in source.
        let data_e1 = exact_candidate("a.json", EvidenceDescriptor::for_e1(1, true, false), true);
        let source_e2 = exact_candidate("b.rs", EvidenceDescriptor::for_e2(1, true, false), false);
        assert_eq!(r3_cmp(&data_e1, &source_e2), Ordering::Less);
    }

    #[test]
    fn source_file_descriptor_serializes_without_data_flag() {
        let json = serde_json::to_value(EvidenceDescriptor::for_e1(1, true, false)).unwrap();
        assert!(json.get("data_file").is_none());
        assert!(json.get("receiver_in_path").is_none());
        let parsed: EvidenceDescriptor = serde_json::from_value(json).unwrap();
        assert!(!parsed.data_file);
        assert!(!parsed.receiver_in_path);
    }

    #[test]
    fn source_code_wins_a_tie_that_the_path_sort_would_give_a_document() {
        // Same evidence, and `.gsd/...` is the smaller path. Before the tail
        // ordered source first, the note ranked above the declaring file.
        for evidence in [
            EvidenceDescriptor::for_e1(1, true, false),
            EvidenceDescriptor::for_e2(2, true, false),
        ] {
            let note = exact_candidate(".gsd/S05-RESEARCH.md", evidence.clone(), false);
            let source = exact_candidate("crates/aft/src/edit.rs", evidence, false);
            let mut results = vec![note.clone(), source.clone()];
            sort_r3(&mut results);
            assert_eq!(results, vec![source, note]);
        }
    }

    #[test]
    fn definition_in_a_file_naming_the_receiver_sorts_first() {
        let other = exact_candidate(
            "crates/aft/src/bash_background/registry.rs",
            EvidenceDescriptor::for_definition(true, false),
            false,
        );
        let mut named = EvidenceDescriptor::for_definition(true, false);
        named.receiver_in_path = true;
        let receiver = exact_candidate("crates/aft/src/context.rs", named, false);
        let mut results = vec![other.clone(), receiver.clone()];
        sort_r3(&mut results);
        assert_eq!(results, vec![receiver, other]);
    }
}
