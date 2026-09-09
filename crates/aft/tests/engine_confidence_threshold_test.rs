use std::fs;
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

#[allow(dead_code)]
#[path = "../src/commands/semantic_search/blocks.rs"]
mod blocks;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/confidence.rs"]
mod confidence;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/scoring.rs"]
mod scoring;

use confidence::{
    Confidence, ConfidenceBranch, ConfidenceCandidate, ConfidenceDecision, ConfidenceEngine,
    ConfidenceThresholdSource, CONFIDENCE_THRESHOLD_RELATIVE_PATH, FLAT_HEAD_LINE,
};
use evidence_descriptor::EvidenceDescriptor;

#[derive(Debug, Deserialize)]
struct Fixture {
    margin: MarginFixture,
}

#[derive(Debug, Deserialize)]
struct MarginFixture {
    within: [f32; 2],
    calibration_flip: [f32; 2],
    separated: [f32; 2],
    calibrated_margin: f32,
    calibrated_model_id: String,
}

#[derive(Debug)]
struct Candidate {
    evidence: EvidenceDescriptor,
    score: Option<f32>,
}

impl Candidate {
    fn non_exact(score: f32) -> Self {
        Self {
            evidence: EvidenceDescriptor::for_non_exact(false, false),
            score: Some(score),
        }
    }

    fn non_exact_with_descriptor(score: f32, exact_form: bool, generated: bool) -> Self {
        Self {
            evidence: EvidenceDescriptor::for_non_exact(exact_form, generated),
            score: Some(score),
        }
    }

    fn exact(evidence: EvidenceDescriptor) -> Self {
        Self {
            evidence,
            score: None,
        }
    }
}

impl ConfidenceCandidate for Candidate {
    fn evidence_descriptor(&self) -> &EvidenceDescriptor {
        &self.evidence
    }

    fn frozen_fusion_score(&self) -> Option<f32> {
        self.score
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn shipped_threshold_path() -> PathBuf {
    workspace_root().join(CONFIDENCE_THRESHOLD_RELATIVE_PATH)
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/confidence/cases.json"
    ))
    .expect("confidence fixture must be valid JSON")
}

fn threshold_tempdir() -> tempfile::TempDir {
    let target = workspace_root().join("target/confidence-threshold-tests");
    fs::create_dir_all(&target).expect("create confidence threshold test parent");
    tempfile::tempdir_in(target).expect("confidence threshold tempdir")
}

fn write_threshold(path: &Path, content: &str) {
    fs::write(path, content).expect("write threshold fixture")
}

fn assert_startup_error(path: &Path, content: &str, expected_field: &'static str) {
    write_threshold(path, content);
    let error = ConfidenceEngine::start(path).expect_err("invalid threshold must fail startup");
    assert_eq!(error.path(), path);
    assert_eq!(error.field(), expected_field);
    let rendered = error.to_string();
    assert!(rendered.contains(&path.display().to_string()));
    assert!(rendered.contains(expected_field));
}

#[test]
fn shipped_threshold_is_the_pinned_provisional_artifact() {
    let path = shipped_threshold_path();
    let bytes = fs::read_to_string(&path).expect("read shipped confidence threshold");
    let value: serde_json::Value = serde_json::from_str(&bytes).expect("valid shipped JSON");
    assert_eq!(
        value,
        serde_json::json!({
            "schema": 1,
            "source": "provisional",
            "margin": 0.05,
            "model_id": null,
            "calibrated_at": null
        })
    );

    let engine = ConfidenceEngine::start(&path).expect("shipped threshold starts");
    assert_eq!(engine.threshold().schema, 1);
    assert_eq!(
        engine.threshold().source,
        ConfidenceThresholdSource::Provisional
    );
    assert_eq!(engine.threshold().margin, 0.05);
    assert_eq!(engine.threshold().model_id, None);
    assert_eq!(engine.threshold().calibrated_at, None);
}

#[test]
fn every_invalid_threshold_is_a_named_startup_error() {
    let directory = threshold_tempdir();
    let path = directory.path().join("confidence-threshold.json");

    let missing = ConfidenceEngine::start(&path).expect_err("missing threshold must fail startup");
    assert_eq!(missing.path(), path);
    assert_eq!(missing.field(), "file");
    assert!(missing.to_string().contains("confidence-threshold.json"));
    assert!(missing.to_string().contains("`file`"));

    let cases = [
        ("{", "json"),
        ("[]", "json"),
        (
            r#"{"schema":2,"source":"provisional","margin":0.05,"model_id":null,"calibrated_at":null}"#,
            "schema",
        ),
        (
            r#"{"schema":1,"source":"provisional","margin":-0.01,"model_id":null,"calibrated_at":null}"#,
            "margin",
        ),
        (
            r#"{"schema":1,"source":"provisional","margin":"NaN","model_id":null,"calibrated_at":null}"#,
            "margin",
        ),
        (
            r#"{"schema":1,"source":"provisional","margin":"Infinity","model_id":null,"calibrated_at":null}"#,
            "margin",
        ),
        (
            r#"{"schema":1,"source":"provisional","margin":1e400,"model_id":null,"calibrated_at":null}"#,
            "margin",
        ),
        (
            r#"{"schema":1,"source":"calibrated","margin":0.12,"model_id":null,"calibrated_at":null}"#,
            "model_id",
        ),
        (
            r#"{"schema":1,"source":"other","margin":0.05,"model_id":null,"calibrated_at":null}"#,
            "source",
        ),
        (
            r#"{"schema":1,"source":"provisional","model_id":null,"calibrated_at":null}"#,
            "margin",
        ),
    ];
    for (content, field) in cases {
        assert_startup_error(&path, content, field);
    }
}

#[test]
fn threshold_is_read_once_per_engine_startup() {
    let values = fixture().margin;
    let directory = threshold_tempdir();
    let path = directory.path().join("confidence-threshold.json");
    let provisional =
        r#"{"schema":1,"source":"provisional","margin":0.05,"model_id":null,"calibrated_at":null}"#;
    write_threshold(&path, provisional);
    let provisional_engine = ConfidenceEngine::start(&path).expect("provisional startup");

    let calibrated = serde_json::json!({
        "schema": 1,
        "source": "calibrated",
        "margin": values.calibrated_margin,
        "model_id": values.calibrated_model_id,
        "calibrated_at": "2026-09-09T00:00:00Z"
    })
    .to_string();
    write_threshold(&path, &calibrated);
    let calibrated_engine = ConfidenceEngine::start(&path).expect("calibrated startup");
    fs::remove_file(&path).expect("remove threshold after both startups");

    let candidates = [
        Candidate::non_exact(values.calibration_flip[0]),
        Candidate::non_exact(values.calibration_flip[1]),
    ];
    let provisional_decision = provisional_engine
        .evaluate_candidates(&candidates)
        .expect("retained provisional threshold");
    let calibrated_decision = calibrated_engine
        .evaluate_candidates(&candidates)
        .expect("retained calibrated threshold");
    assert_eq!(provisional_decision.confidence, Some(Confidence::High));
    assert_eq!(provisional_decision.flat_head_line, None);
    assert_eq!(calibrated_decision.confidence, Some(Confidence::Low));
    assert_eq!(calibrated_decision.flat_head_line, Some(FLAT_HEAD_LINE));
    assert_eq!(
        provisional_decision.branch,
        ConfidenceBranch::NonExactMargin
    );
    assert_eq!(calibrated_decision.branch, ConfidenceBranch::NonExactMargin);

    let missing_after_start = ConfidenceEngine::start(&path)
        .expect_err("a new startup must not reuse another engine's threshold");
    assert_eq!(missing_after_start.field(), "file");
}

struct CalibrationCase {
    label: &'static str,
    candidates: Vec<Candidate>,
    threshold_sensitive: bool,
}

fn decision(engine: &ConfidenceEngine, case: &CalibrationCase) -> ConfidenceDecision {
    engine
        .evaluate_candidates(&case.candidates)
        .unwrap_or_else(|error| panic!("{} confidence failed: {error}", case.label))
}

#[test]
fn calibrated_artifact_changes_only_margin_fixtures_without_switching_branch() {
    let values = fixture().margin;
    let directory = threshold_tempdir();
    let provisional_path = directory.path().join("provisional.json");
    let calibrated_path = directory.path().join("calibrated.json");
    write_threshold(
        &provisional_path,
        r#"{"schema":1,"source":"provisional","margin":0.05,"model_id":null,"calibrated_at":null}"#,
    );
    write_threshold(
        &calibrated_path,
        &serde_json::json!({
            "schema": 1,
            "source": "calibrated",
            "margin": values.calibrated_margin,
            "model_id": values.calibrated_model_id,
            "calibrated_at": null
        })
        .to_string(),
    );
    let provisional = ConfidenceEngine::start(&provisional_path).expect("provisional engine");
    let calibrated = ConfidenceEngine::start(&calibrated_path).expect("calibrated engine");

    let cases = [
        CalibrationCase {
            label: "calibration-flip",
            candidates: vec![
                Candidate::non_exact(values.calibration_flip[0]),
                Candidate::non_exact(values.calibration_flip[1]),
            ],
            threshold_sensitive: true,
        },
        CalibrationCase {
            label: "within-both",
            candidates: vec![
                Candidate::non_exact(values.within[0]),
                Candidate::non_exact(values.within[1]),
            ],
            threshold_sensitive: false,
        },
        CalibrationCase {
            label: "separated-both",
            candidates: vec![
                Candidate::non_exact(values.separated[0]),
                Candidate::non_exact(values.separated[1]),
            ],
            threshold_sensitive: false,
        },
        CalibrationCase {
            label: "descriptor-separation",
            candidates: vec![
                Candidate::non_exact_with_descriptor(0.5, true, false),
                Candidate::non_exact_with_descriptor(0.5, false, false),
            ],
            threshold_sensitive: false,
        },
        CalibrationCase {
            label: "exact-flat",
            candidates: vec![
                Candidate::exact(EvidenceDescriptor::for_e1(2, true, false)),
                Candidate::exact(EvidenceDescriptor::for_e1(2, false, true)),
            ],
            threshold_sensitive: false,
        },
        CalibrationCase {
            label: "exact-separated",
            candidates: vec![
                Candidate::exact(EvidenceDescriptor::for_definition(false, true)),
                Candidate::exact(EvidenceDescriptor::for_e1(20, true, false)),
            ],
            threshold_sensitive: false,
        },
        CalibrationCase {
            label: "singleton",
            candidates: vec![Candidate::non_exact(values.calibration_flip[0])],
            threshold_sensitive: false,
        },
    ];

    for case in cases {
        let before = decision(&provisional, &case);
        let after = decision(&calibrated, &case);
        assert_eq!(
            before.branch, after.branch,
            "{} switched confidence decision branches",
            case.label
        );
        assert_eq!(
            before.confidence != after.confidence,
            case.threshold_sensitive,
            "{} had the wrong calibration sensitivity",
            case.label
        );
        assert_eq!(
            before.flat_head_line != after.flat_head_line,
            case.threshold_sensitive,
            "{} had the wrong flat-head-line sensitivity",
            case.label
        );
    }
}
