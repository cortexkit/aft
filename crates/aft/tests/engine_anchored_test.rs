use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use aft::commands::semantic_search::comparator::{r3_cmp, CandidateResult, RankedTuple};
use aft::commands::semantic_search::evidence_descriptor::{
    EvidenceDescriptor, EvidenceKind, EvidenceTier,
};
use aft::search_index::SearchIndex;
use aft::commands::semantic_search::anchored_lane::{
    cmp_alignment, discover_anchored_candidate_files, find_canonical_alignment,
    find_canonical_alignment_with_test_order, find_run_occurrences, sort_anchored_canonical,
    split_query, verify_anchored_text, Alignment, AlignmentTestOrder, AnchoredLane, Occurrence,
    MAX_RUN_OCCURRENCES,
};

const FIXTURE_NAMES: [&str; 9] = [
    "fixture_i_phrase_lane_unreachable.json",
    "fixture_ii_decoy_rejected_on_evidence.json",
    "fixture_iii_single_run_qualification.json",
    "fixture_iv_conjunction_negative.json",
    "fixture_v_zero_runs_negative.json",
    "fixture_vi_too_short_run_negative.json",
    "fixture_vii_gap_tie_break.json",
    "fixture_viii_repeated_runs_canonical_alignment.json",
    "fixture_ix_intermediate_occurrence_totality.json",
];

#[derive(Clone, Debug, serde::Deserialize)]
struct CandidateFixture {
    path: PathBuf,
    matched_total: usize,
    gap_chars: usize,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct AnchoredFixture {
    name: String,
    query: String,
    source_text: String,
    retained_runs: Vec<String>,
    retained_run_lengths: Vec<usize>,
    matched_occurrence_offsets: Vec<usize>,
    matched_occurrences: Vec<(usize, usize)>,
    matched_total: usize,
    denominator: usize,
    threshold_outcome: bool,
    #[serde(default)]
    gap_chars: usize,
    #[serde(default)]
    candidate_a: Option<CandidateFixture>,
    #[serde(default)]
    candidate_b: Option<CandidateFixture>,
    #[serde(default)]
    expected_order: Vec<PathBuf>,
    #[serde(default)]
    competitor_gap: Option<usize>,
}

impl AnchoredFixture {
    fn validate_integers(&self) -> Result<(), String> {
        if self.retained_runs.len() != self.retained_run_lengths.len() {
            return Err(format!(
                "integer disagreement in {}: {} runs but {} run lengths",
                self.name,
                self.retained_runs.len(),
                self.retained_run_lengths.len()
            ));
        }

        for (run_index, (run, stated_len)) in self
            .retained_runs
            .iter()
            .zip(&self.retained_run_lengths)
            .enumerate()
        {
            if run.len() != *stated_len {
                return Err(format!(
                    "integer disagreement in {} run {run_index}: literal len {} but stated len {stated_len}",
                    self.name,
                    run.len()
                ));
            }
        }

        let literal_denominator = self.retained_runs.iter().map(String::len).sum::<usize>();
        if literal_denominator != self.denominator {
            return Err(format!(
                "integer disagreement in {}: literal-run total {literal_denominator} but stated denominator {}",
                self.name, self.denominator
            ));
        }

        let occurrence_offsets = self
            .matched_occurrences
            .iter()
            .map(|(_, offset)| *offset)
            .collect::<Vec<_>>();
        if occurrence_offsets != self.matched_occurrence_offsets {
            return Err(format!(
                "integer disagreement in {}: occurrence offsets {:?} but stated offsets {:?}",
                self.name, occurrence_offsets, self.matched_occurrence_offsets
            ));
        }

        Ok(())
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn load_fixture(name: &str) -> AnchoredFixture {
    let path = workspace_root()
        .join("benchmarks/aft-search/engine-fixtures/anchored")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&path).expect("read anchored fixture"))
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn assert_five_fixture_values(
    name: &str,
) -> (AnchoredFixture, aft::commands::semantic_search::anchored_lane::AnchoredVerification) {
    let fixture = load_fixture(name);
    fixture
        .validate_integers()
        .unwrap_or_else(|error| panic!("{error}"));
    let actual = verify_anchored_text(&fixture.query, &fixture.source_text);

    assert_eq!(
        actual.retained_runs, fixture.retained_runs,
        "{} retained run list",
        fixture.name
    );
    assert_eq!(
        actual.retained_run_lengths, fixture.retained_run_lengths,
        "{} literal run len() values",
        fixture.name
    );
    assert_eq!(
        actual.matched_occurrences, fixture.matched_occurrences,
        "{} canonical matched occurrence offsets",
        fixture.name
    );
    assert_eq!(
        actual.matched_total, fixture.matched_total,
        "{} matched-run character total",
        fixture.name
    );
    assert_eq!(
        actual.denominator, fixture.denominator,
        "{} retained-run denominator",
        fixture.name
    );
    assert_eq!(
        actual.threshold_outcome, fixture.threshold_outcome,
        "{} threshold outcome",
        fixture.name
    );

    (fixture, actual)
}

fn index_file(index: &mut SearchIndex, path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, contents).expect("write indexed fixture");
    index.index_file(path, contents.as_bytes());
}

#[test]
fn fixture_integrity_rejects_stated_integer_before_ratio_evaluation() {
    for name in FIXTURE_NAMES {
        load_fixture(name)
            .validate_integers()
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }

    let mut bad = load_fixture(FIXTURE_NAMES[0]);
    bad.denominator -= 1;
    let error = bad.validate_integers().expect_err("bad integer must fail");
    assert!(error.contains("integer disagreement"));
    assert!(error.contains("stated denominator"));

    let mut bad_run_len = load_fixture(FIXTURE_NAMES[0]);
    bad_run_len.retained_run_lengths[1] -= 1;
    let error = bad_run_len
        .validate_integers()
        .expect_err("bad literal length must fail");
    assert!(error.contains("literal len"));
    assert!(!error.contains("ratio"));
}

#[test]
fn fixture_i_phrase_lane_unreachable_log_line() {
    let (fixture, _) = assert_five_fixture_values(FIXTURE_NAMES[0]);
    let temp = tempfile::tempdir().expect("temporary corpus");
    let corpus = temp.path().join("corpus");
    let server_path = corpus.join("src/server.rs");
    let decoy_path = corpus.join("src/decoy.rs");
    let test_path = corpus.join("tests/server_test.rs");
    let outside_path = temp.path().join("outside/server.rs");
    let mut index = SearchIndex::new();

    index_file(&mut index, &server_path, &fixture.source_text);
    index_file(
        &mut index,
        &decoy_path,
        "info!(\"opening connection to database\");\n",
    );
    index_file(&mut index, &test_path, &fixture.source_text);
    index_file(&mut index, &outside_path, &fixture.source_text);

    let split = split_query(&fixture.query);
    let without_tests =
        discover_anchored_candidate_files(&index, &corpus, &split.retained_runs, false);
    assert!(without_tests.contains(&server_path));
    assert!(without_tests.contains(&decoy_path));
    assert!(!without_tests.contains(&test_path));
    assert!(!without_tests.contains(&outside_path));

    let with_tests = discover_anchored_candidate_files(&index, &corpus, &split.retained_runs, true);
    assert!(with_tests.contains(&test_path));

    let results = AnchoredLane::new().execute_ready_mode(&index, &corpus, &fixture.query, false);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].path, server_path,
        "qualifying anchored hit ranks 1"
    );
    assert_eq!(results[0].evidence.matched_span, Some(21));
    assert_eq!(results[0].evidence.tier, EvidenceTier::Exact);
    assert_eq!(results[0].evidence.kind, EvidenceKind::Anchored);
}

#[test]
fn fixture_ii_decoy_is_discovered_then_rejected_on_evidence() {
    let (fixture, _) = assert_five_fixture_values(FIXTURE_NAMES[1]);
    let temp = tempfile::tempdir().expect("temporary corpus");
    let decoy_path = temp.path().join("decoy.rs");
    let mut index = SearchIndex::new();
    index_file(&mut index, &decoy_path, &fixture.source_text);

    let split = split_query(&fixture.query);
    let discovered =
        discover_anchored_candidate_files(&index, temp.path(), &split.retained_runs, true);
    assert!(
        discovered.contains(&decoy_path),
        "the union must discover a file containing only one retained run"
    );

    let results = AnchoredLane::new().execute_ready_mode(&index, temp.path(), &fixture.query, true);
    assert!(
        results.is_empty(),
        "7/21 evidence must be rejected after discovery"
    );
}

#[test]
fn fixture_iii_single_run_qualification() {
    assert_five_fixture_values(FIXTURE_NAMES[2]);
}

#[test]
fn fixture_iv_sixty_percent_and_twelve_char_clauses_are_a_conjunction() {
    assert_five_fixture_values(FIXTURE_NAMES[3]);
}

#[test]
fn fixture_v_zero_retained_runs_is_negative() {
    assert_five_fixture_values(FIXTURE_NAMES[4]);
}

#[test]
fn fixture_vi_too_short_run_is_negative() {
    assert_five_fixture_values(FIXTURE_NAMES[5]);
}

#[test]
fn fixture_vii_gap_tie_break_is_shared_by_lane_and_final_comparator() {
    let (fixture, actual) = assert_five_fixture_values(FIXTURE_NAMES[6]);
    assert_eq!(actual.gap_chars, fixture.gap_chars);

    let candidate_a = fixture.candidate_a.expect("candidate A");
    let candidate_b = fixture.candidate_b.expect("candidate B");
    let build_candidate = |candidate: &CandidateFixture| {
        CandidateResult::new_exact(
            candidate.path.clone(),
            None,
            EvidenceDescriptor::for_anchored(
                candidate.matched_total,
                candidate.gap_chars,
                true,
                false,
            ),
        )
    };

    let mut lane_order = vec![build_candidate(&candidate_a), build_candidate(&candidate_b)];
    sort_anchored_canonical(&mut lane_order);
    let mut final_order = vec![build_candidate(&candidate_a), build_candidate(&candidate_b)];
    final_order.sort_by(r3_cmp);

    let lane_paths = lane_order
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<Vec<_>>();
    let final_paths = final_order
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<Vec<_>>();
    assert_eq!(lane_paths, fixture.expected_order);
    assert_eq!(final_paths, fixture.expected_order);
    assert_eq!(lane_order, final_order);
}

#[test]
fn fixture_viii_repeated_runs_choose_the_canonical_anchor_window() {
    let (fixture, actual) = assert_five_fixture_values(FIXTURE_NAMES[7]);
    assert_eq!(actual.gap_chars, fixture.gap_chars);

    let alignment = find_canonical_alignment(&fixture.source_text, &fixture.retained_runs)
        .expect("canonical repeated-run alignment");
    assert_eq!(alignment.occurrence_list(), fixture.matched_occurrences);
    assert_eq!(alignment.gap_chars, 2);

    let adjacent = CandidateResult::new_exact(
        PathBuf::from("z_adjacent.rs"),
        None,
        EvidenceDescriptor::for_anchored(alignment.matched_total, alignment.gap_chars, true, false),
    );
    let competitor = CandidateResult::new_exact(
        PathBuf::from("a_competitor.rs"),
        None,
        EvidenceDescriptor::for_anchored(
            alignment.matched_total,
            fixture.competitor_gap.expect("competitor gap"),
            true,
            false,
        ),
    );
    let mut final_order = vec![competitor, adjacent];
    final_order.sort_by(r3_cmp);
    assert_eq!(final_order[0].path, PathBuf::from("z_adjacent.rs"));

    let cross_function =
        "run_alpha_start\n// second line\n// third line\n// fourth line\nrun_omega_end\n";
    let bounded = find_canonical_alignment(cross_function, &fixture.retained_runs)
        .expect("at least one retained run occurs");
    assert_eq!(
        bounded.runs_matched(),
        1,
        "one alignment must not cross a three-line anchor window"
    );
}

#[test]
fn fixture_ix_intermediate_occurrence_totality_is_enumeration_independent() {
    let (fixture, actual) = assert_five_fixture_values(FIXTURE_NAMES[8]);
    assert_eq!(actual.gap_chars, fixture.gap_chars);

    let forward = find_canonical_alignment(&fixture.source_text, &fixture.retained_runs)
        .expect("forward canonical alignment");
    let reversed = find_canonical_alignment_with_test_order(
        &fixture.source_text,
        &fixture.retained_runs,
        AlignmentTestOrder {
            reverse_window_scan: true,
            reverse_occurrence_enumeration: true,
        },
    )
    .expect("reverse-fed canonical alignment");
    assert_eq!(forward.occurrence_list(), fixture.matched_occurrences);
    assert_eq!(reversed.occurrence_list(), fixture.matched_occurrences);

    let earlier_run_subset = Alignment {
        occurrences: vec![
            Occurrence {
                run_idx: 1,
                start: 0,
                end: 4,
                len: 4,
            },
            Occurrence {
                run_idx: 3,
                start: 5,
                end: 9,
                len: 4,
            },
        ],
        matched_total: 8,
        gap_chars: 1,
    };
    let later_run_subset = Alignment {
        occurrences: vec![
            Occurrence {
                run_idx: 2,
                start: 0,
                end: 4,
                len: 4,
            },
            Occurrence {
                run_idx: 3,
                start: 5,
                end: 9,
                len: 4,
            },
        ],
        matched_total: 8,
        gap_chars: 1,
    };
    assert_eq!(
        cmp_alignment(&earlier_run_subset, &later_run_subset),
        Ordering::Greater,
        "zeta compares (run index, start offset), not start offsets alone"
    );
}

#[test]
fn grammar_treats_all_log_levels_and_pid_brackets_as_variable_spans() {
    for level in [
        "INFO", "warn", "Error", "dEbUg", "TRACE", "fatal", "PANICKED",
    ] {
        let split = split_query(&format!("{level} opening [4821] failed safely"));
        assert_eq!(split.retained_runs, vec!["opening", "failed safely"]);
        assert_eq!(split.retained_run_lengths, vec![7, 13]);
        assert_eq!(split.denominator, 20);
    }

    let whole_token = split_query("warning opening [4821] failed safely");
    assert_eq!(
        whole_token.retained_runs,
        vec!["warning opening", "failed safely"],
        "WARN inside a larger word is literal text"
    );
}

#[test]
fn occurrence_cap_uses_the_first_64_ascending_offsets() {
    let text = "target_run ".repeat(80);
    let occurrences = find_run_occurrences(&text, "target_run", 1);
    assert_eq!(occurrences.len(), MAX_RUN_OCCURRENCES);
    assert_eq!(occurrences.first().map(|occ| occ.start), Some(0));
    assert_eq!(occurrences.last().map(|occ| occ.start), Some(63 * 11));
    assert!(occurrences
        .windows(2)
        .all(|pair| pair[0].start < pair[1].start));

    let runs = vec!["target_run".to_string()];
    let forward = find_canonical_alignment(&text, &runs).expect("forward alignment");
    let reverse_fed = find_canonical_alignment_with_test_order(
        &text,
        &runs,
        AlignmentTestOrder {
            reverse_occurrence_enumeration: true,
            ..Default::default()
        },
    )
    .expect("reverse-fed alignment");
    assert_eq!(forward.occurrence_list(), vec![(1, 0)]);
    assert_eq!(reverse_fed.occurrence_list(), forward.occurrence_list());
}

#[test]
fn every_emitted_anchored_result_is_exact_tier_and_score_free() {
    let fixture = load_fixture(FIXTURE_NAMES[0]);
    let temp = tempfile::tempdir().expect("temporary corpus");
    let mut index = SearchIndex::new();
    for name in ["b.rs", "a.rs"] {
        index_file(&mut index, &temp.path().join(name), &fixture.source_text);
    }

    let results = AnchoredLane::new().execute_ready_mode(&index, temp.path(), &fixture.query, true);
    assert_eq!(results.len(), 2);
    for (r3_order_index, candidate) in results.iter().enumerate() {
        assert_eq!(candidate.evidence.tier, EvidenceTier::Exact);
        assert_eq!(candidate.evidence.kind, EvidenceKind::Anchored);
        assert_eq!(candidate.fusion_score, None);
        assert_eq!(candidate.lane_score, None);

        let candidate_json = serde_json::to_value(candidate).expect("serialize candidate");
        assert!(candidate_json.get("fusion_score").is_none());
        assert!(candidate_json.get("lane_score").is_none());

        let tuple = RankedTuple {
            file: candidate.path.clone(),
            symbol_range: candidate.symbol_range,
            r3_order_index,
            fusion_score: candidate.fusion_score,
            lane_score: candidate.lane_score,
        };
        let tuple_json = serde_json::to_value(tuple).expect("serialize ranked tuple");
        assert!(tuple_json.get("fusion_score").is_none());
        assert!(tuple_json.get("lane_score").is_none());
    }
}
