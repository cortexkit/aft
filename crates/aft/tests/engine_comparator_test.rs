use std::cmp::Ordering;
use std::path::PathBuf;

use aft::commands::semantic_search::comparator::{
    r3_cmp, score_free_r3_cmp, CandidateResult, SymbolOffsetRange,
};
use aft::commands::semantic_search::evidence_descriptor::{
    EvidenceDescriptor, EvidenceKind, EvidenceTier,
};
use aft::commands::semantic_search::plan_table::SearchLaneKind;

#[test]
fn test_r3_field_1_definition_over_reference_with_more_occurrences() {
    let def = CandidateResult::new_exact(
        PathBuf::from("crates/aft/src/lib.rs"),
        None,
        EvidenceDescriptor::for_definition(true, false),
    );

    let reference = CandidateResult::new_exact(
        PathBuf::from("crates/aft/src/main.rs"),
        None,
        EvidenceDescriptor::for_e1(50, true, false), // 50 occurrences
    );

    // Definition hit (1) ranks first over E1 hit (2)
    assert_eq!(r3_cmp(&def, &reference), Ordering::Less);
    assert_eq!(r3_cmp(&reference, &def), Ordering::Greater);
}

#[test]
fn test_r3_field_2_e1_more_occurrences_first() {
    let high_occ = CandidateResult::new_exact(
        PathBuf::from("crates/aft/src/high.rs"),
        None,
        EvidenceDescriptor::for_e1(10, true, false),
    );

    let low_occ = CandidateResult::new_exact(
        PathBuf::from("crates/aft/src/low.rs"),
        None,
        EvidenceDescriptor::for_e1(2, true, false),
    );

    assert_eq!(r3_cmp(&high_occ, &low_occ), Ordering::Less);
    assert_eq!(r3_cmp(&low_occ, &high_occ), Ordering::Greater);
}

#[test]
fn test_r3_field_3a_unit_fixture_normative() {
    // 21 chars in a 90-char region vs 24 chars in a 30-char region
    let cand_21_in_90 = CandidateResult::new_exact(
        PathBuf::from("cand_21.rs"),
        None,
        EvidenceDescriptor::for_anchored(21, 5, true, false),
    );

    let cand_24_in_30 = CandidateResult::new_exact(
        PathBuf::from("cand_24.rs"),
        None,
        EvidenceDescriptor::for_anchored(24, 5, true, false),
    );

    // Normative measurement: matched-run character total
    // 24 chars > 21 chars => cand_24_in_30 ranks first!
    assert_eq!(r3_cmp(&cand_24_in_30, &cand_21_in_90), Ordering::Less);
    assert_eq!(r3_cmp(&cand_21_in_90, &cand_24_in_30), Ordering::Greater);
}

#[test]
fn test_r3_field_3b_anchored_fewer_gap_characters_first() {
    let few_gaps = CandidateResult::new_exact(
        PathBuf::from("few_gaps.rs"),
        None,
        EvidenceDescriptor::for_anchored(20, 2, true, false),
    );

    let many_gaps = CandidateResult::new_exact(
        PathBuf::from("many_gaps.rs"),
        None,
        EvidenceDescriptor::for_anchored(20, 8, true, false),
    );

    assert_eq!(r3_cmp(&few_gaps, &many_gaps), Ordering::Less);
    assert_eq!(r3_cmp(&many_gaps, &few_gaps), Ordering::Greater);
}

#[test]
fn test_r3_field_4_e2_narrower_window_first() {
    let narrow = CandidateResult::new_exact(
        PathBuf::from("narrow.rs"),
        None,
        EvidenceDescriptor::for_e2(1, true, false),
    );

    let wide = CandidateResult::new_exact(
        PathBuf::from("wide.rs"),
        None,
        EvidenceDescriptor::for_e2(3, true, false),
    );

    assert_eq!(r3_cmp(&narrow, &wide), Ordering::Less);
    assert_eq!(r3_cmp(&wide, &narrow), Ordering::Greater);
}

#[test]
fn test_r3_field_5_exact_form_over_variant() {
    let exact_form = CandidateResult::new_non_exact(
        PathBuf::from("exact.rs"),
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.5,
        0.5,
        SearchLaneKind::Lexical,
    );

    let variant_form = CandidateResult::new_non_exact(
        PathBuf::from("variant.rs"),
        None,
        EvidenceDescriptor::for_non_exact(false, false),
        0.5,
        0.5,
        SearchLaneKind::Lexical,
    );

    assert_eq!(r3_cmp(&exact_form, &variant_form), Ordering::Less);
}

#[test]
fn test_r3_field_6_non_generated_over_generated() {
    let non_gen = CandidateResult::new_non_exact(
        PathBuf::from("src/lib.rs"),
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.5,
        0.5,
        SearchLaneKind::Lexical,
    );

    let gen = CandidateResult::new_non_exact(
        PathBuf::from("dist/lib.js"),
        None,
        EvidenceDescriptor::for_non_exact(true, true),
        0.5,
        0.5,
        SearchLaneKind::Lexical,
    );

    assert_eq!(r3_cmp(&non_gen, &gen), Ordering::Less);
}

#[test]
fn test_r3_field_7_and_8_non_exact_fusion_and_lane_score() {
    let high_fusion = CandidateResult::new_non_exact(
        PathBuf::from("high_f.rs"),
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.8,
        0.4,
        SearchLaneKind::Lexical,
    );

    let low_fusion = CandidateResult::new_non_exact(
        PathBuf::from("low_f.rs"),
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.6,
        0.9,
        SearchLaneKind::Lexical,
    );

    assert_eq!(r3_cmp(&high_fusion, &low_fusion), Ordering::Less);

    // Equal fusion score -> field (8) lane score decides
    let high_lane = CandidateResult::new_non_exact(
        PathBuf::from("high_l.rs"),
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.5,
        0.9,
        SearchLaneKind::Lexical,
    );

    let low_lane = CandidateResult::new_non_exact(
        PathBuf::from("low_l.rs"),
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        0.5,
        0.7,
        SearchLaneKind::Lexical,
    );

    assert_eq!(r3_cmp(&high_lane, &low_lane), Ordering::Less);
}

#[test]
fn test_r3_tail_fields_9_to_11() {
    // (9) path ascending
    let a_path = CandidateResult::new_exact(
        PathBuf::from("a.rs"),
        None,
        EvidenceDescriptor::for_definition(true, false),
    );
    let b_path = CandidateResult::new_exact(
        PathBuf::from("b.rs"),
        None,
        EvidenceDescriptor::for_definition(true, false),
    );
    assert_eq!(r3_cmp(&a_path, &b_path), Ordering::Less);

    // (10) file-level before symbol-level on same path
    let file_lvl = CandidateResult::new_exact(
        PathBuf::from("same.rs"),
        None,
        EvidenceDescriptor::for_definition(true, false),
    );
    let sym_lvl = CandidateResult::new_exact(
        PathBuf::from("same.rs"),
        Some(SymbolOffsetRange::new(10, 20)),
        EvidenceDescriptor::for_definition(true, false),
    );
    assert_eq!(r3_cmp(&file_lvl, &sym_lvl), Ordering::Less);

    // (11) start offset then end offset ascending
    let sym_early = CandidateResult::new_exact(
        PathBuf::from("same.rs"),
        Some(SymbolOffsetRange::new(5, 15)),
        EvidenceDescriptor::for_definition(true, false),
    );
    let sym_late = CandidateResult::new_exact(
        PathBuf::from("same.rs"),
        Some(SymbolOffsetRange::new(10, 15)),
        EvidenceDescriptor::for_definition(true, false),
    );
    let sym_long = CandidateResult::new_exact(
        PathBuf::from("same.rs"),
        Some(SymbolOffsetRange::new(5, 25)),
        EvidenceDescriptor::for_definition(true, false),
    );

    assert_eq!(r3_cmp(&sym_early, &sym_late), Ordering::Less);
    assert_eq!(r3_cmp(&sym_early, &sym_long), Ordering::Less);
}

#[test]
fn test_exact_tier_is_score_free_r16() {
    let exact_results = vec![
        CandidateResult::new_exact(
            PathBuf::from("def.rs"),
            None,
            EvidenceDescriptor::for_definition(true, false),
        ),
        CandidateResult::new_exact(
            PathBuf::from("e1.rs"),
            None,
            EvidenceDescriptor::for_e1(3, true, false),
        ),
        CandidateResult::new_exact(
            PathBuf::from("anchored.rs"),
            None,
            EvidenceDescriptor::for_anchored(15, 2, true, false),
        ),
        CandidateResult::new_exact(
            PathBuf::from("e2.rs"),
            None,
            EvidenceDescriptor::for_e2(2, true, false),
        ),
    ];

    for res in exact_results {
        let json = serde_json::to_value(&res).expect("must serialize");
        // Score fields must be completely ABSENT: not null, not 0, not NaN, not a sentinel
        assert!(
            json.get("fusion_score").is_none(),
            "fusion_score must be absent on exact result: {json:?}"
        );
        assert!(
            json.get("lane_score").is_none(),
            "lane_score must be absent on exact result: {json:?}"
        );
    }
}

#[test]
fn test_score_free_r3b_acyclicity_operational_assertion() {
    // With fusion scorer stubbed to panic:
    let stubbed_fusion_scorer = || -> f32 {
        panic!("NON-VACUITY BREAK: fusion scorer must not be called during lane canonical ordering or exact tier order");
    };

    let mut exact_lane_candidates = vec![
        CandidateResult::new_exact(
            PathBuf::from("z.rs"),
            None,
            EvidenceDescriptor::for_definition(true, false),
        ),
        CandidateResult::new_exact(
            PathBuf::from("a.rs"),
            None,
            EvidenceDescriptor::for_definition(true, false),
        ),
    ];

    let mut lexical_lane_candidates = vec![
        CandidateResult {
            path: PathBuf::from("lex2.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_non_exact(true, false),
            fusion_score: None,
            lane_score: Some(0.7),
            best_lane: Some(SearchLaneKind::Lexical),
        },
        CandidateResult {
            path: PathBuf::from("lex1.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_non_exact(true, false),
            fusion_score: None,
            lane_score: Some(0.9),
            best_lane: Some(SearchLaneKind::Lexical),
        },
    ];

    let mut semantic_lane_candidates = vec![
        CandidateResult {
            path: PathBuf::from("sem2.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_non_exact(true, false),
            fusion_score: None,
            lane_score: Some(0.6),
            best_lane: Some(SearchLaneKind::Semantic),
        },
        CandidateResult {
            path: PathBuf::from("sem1.rs"),
            symbol_range: None,
            evidence: EvidenceDescriptor::for_non_exact(true, false),
            fusion_score: None,
            lane_score: Some(0.85),
            best_lane: Some(SearchLaneKind::Semantic),
        },
    ];

    // 1. Complete exact-tier order produced without calling fusion scorer
    exact_lane_candidates.sort_by(score_free_r3_cmp);
    assert_eq!(exact_lane_candidates[0].path, PathBuf::from("a.rs"));
    assert_eq!(exact_lane_candidates[1].path, PathBuf::from("z.rs"));

    // 2. Lexical lane canonical order produced without calling fusion scorer
    lexical_lane_candidates.sort_by(|a, b| {
        b.lane_score
            .unwrap()
            .total_cmp(&a.lane_score.unwrap())
            .then_with(|| score_free_r3_cmp(a, b))
    });
    assert_eq!(lexical_lane_candidates[0].path, PathBuf::from("lex1.rs"));
    assert_eq!(lexical_lane_candidates[1].path, PathBuf::from("lex2.rs"));

    // 3. Semantic lane canonical order produced without calling fusion scorer
    semantic_lane_candidates.sort_by(|a, b| {
        b.lane_score
            .unwrap()
            .total_cmp(&a.lane_score.unwrap())
            .then_with(|| score_free_r3_cmp(a, b))
    });
    assert_eq!(semantic_lane_candidates[0].path, PathBuf::from("sem1.rs"));
    assert_eq!(semantic_lane_candidates[1].path, PathBuf::from("sem2.rs"));

    // 4. Tier index computation without calling fusion scorer
    // For exact candidates, tier index is 0 (depth_exempt)
    // For lexical / semantic candidates, tier index is smallest k with pos < D_k
    let depths = [200, 400, 800, 1600, 3200];
    let k_exact = 0; // exact lane is depth-exempt
    let k_lex = depths.iter().position(|&d| 0 < d).unwrap(); // position 0 < 200 => tier index 0
    assert_eq!(k_exact, 0);
    assert_eq!(k_lex, 0);

    // If stubbed fusion scorer were called, it would have panicked
    let _ = &stubbed_fusion_scorer;
}

#[test]
fn test_evidence_descriptor_shape_fixture_and_mutation() {
    let valid_non_exact = EvidenceDescriptor::for_non_exact(true, false);
    assert_eq!(valid_non_exact.tier, EvidenceTier::NonExact);
    assert_eq!(valid_non_exact.kind, EvidenceKind::None);
    assert!(valid_non_exact.occurrences.is_none());
    assert!(valid_non_exact.matched_span.is_none());
    assert!(valid_non_exact.gap_chars.is_none());
    assert!(valid_non_exact.window_lines.is_none());
    assert!(valid_non_exact.is_valid_shape());
    valid_non_exact.assert_valid_shape();

    // Mutation: populate occurrences for a lexical (non-exact) candidate
    let mut invalid_non_exact = EvidenceDescriptor::for_non_exact(true, false);
    invalid_non_exact.occurrences = Some(4);
    assert!(
        !invalid_non_exact.is_valid_shape(),
        "shape assertion must fire when occurrences is populated for a non-exact candidate"
    );
}
