use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use aft::commands::semantic_search::{
    blocks, comparator, evidence_descriptor, generation_token, paging, plan_table, provenance,
    scoring, trailer,
};

use aft::commands::semantic_search::telemetry::{
    ConfidenceTelemetry, StructuredContent, TelemetryAssembler, TelemetryRun,
};
use blocks::{BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate};
use comparator::sort_r3;
use evidence_descriptor::EvidenceDescriptor;
use generation_token::GenerationToken;
use paging::{parse_public_page_request, serve_public_page, SearchPage};
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use provenance::{LanePositions, LanePositionsAccessor, ObservedProvenance, ProvenanceError};
use scoring::ScoringPolicy;
use trailer::ExactPassState;

#[derive(Debug, Deserialize)]
struct Fixture {
    ready: ModeFixture,
    fallback: ModeFixture,
}

#[derive(Debug, Deserialize)]
struct ModeFixture {
    shape: String,
    confidence: String,
    variants: Vec<String>,
    embedding_calls: usize,
    snapshot_generation: String,
    retrieval_depth: usize,
    depth_tier: usize,
    lanes_exhausted: bool,
    stability_void: bool,
    trailer: String,
    #[serde(default)]
    bounded_disclosure: Option<String>,
    stability_files: Vec<String>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/provenance/telemetry.json"
    ))
    .expect("provenance telemetry fixture")
}

fn deterministic_token(generation: &str, nonce_byte: u8) -> GenerationToken {
    GenerationToken::new_with_nonce(generation, [nonce_byte; 16])
}

fn policy(shape: SearchShape) -> ScoringPolicy {
    ScoringPolicy::from_plan_table(&PlanTable::running_table(), shape)
        .expect("fixture scoring policy")
}

fn non_exact(path: &str, score: f32) -> LaneCandidate {
    LaneCandidate::non_exact(
        path,
        None,
        EvidenceDescriptor::for_non_exact(true, false),
        score,
        false,
    )
}

fn ready_page() -> (SearchPage, GenerationToken) {
    let token = deterministic_token("ready-generation", 0x11);
    let exact_path = PathBuf::from("ready-exact.rs");
    let non_exact_path = PathBuf::from("ready-non-exact.rs");
    let lanes = vec![
        CanonicalLane::new(
            SearchLaneKind::Semantic,
            vec![non_exact("ready-non-exact.rs", 0.9)],
        )
        .expect("semantic lane"),
        CanonicalLane::new(
            SearchLaneKind::Exact,
            vec![LaneCandidate::exact(
                exact_path.clone(),
                None,
                EvidenceDescriptor::for_e1(2, true, false),
                false,
            )],
        )
        .expect("exact lane"),
        CanonicalLane::new(
            SearchLaneKind::Lexical,
            vec![
                non_exact("ready-exact.rs", 1.0),
                LaneCandidate::non_exact(
                    non_exact_path,
                    None,
                    EvidenceDescriptor::for_non_exact(true, false),
                    0.8,
                    false,
                ),
            ],
        )
        .expect("lexical lane"),
    ];
    let builder = BlockBuilder::new(
        CanonicalListKey {
            project_root: PathBuf::from("/ready-fixture"),
            snapshot_generation: token.to_string(),
            normalized_query: "opening failed".to_string(),
            include_tests: false,
        },
        policy(SearchShape::NaturalLanguage),
        lanes,
    )
    .expect("ready builder");
    let request =
        parse_public_page_request(&json!({"offset": 0, "topK": 2})).expect("ready page request");
    let page = serve_public_page(&builder, request).expect("ready page");
    (page, token)
}

fn fallback_page() -> (SearchPage, GenerationToken) {
    let token = deterministic_token("fallback-generation", 0x22);
    let lanes = vec![
        CanonicalLane::new(SearchLaneKind::Exact, Vec::new()).expect("empty exact lane"),
        CanonicalLane::new(SearchLaneKind::Lexical, vec![non_exact("fallback.rs", 0.7)])
            .expect("fallback lexical lane"),
    ];
    let builder = BlockBuilder::new(
        CanonicalListKey {
            project_root: PathBuf::from("/fallback-fixture"),
            snapshot_generation: token.to_string(),
            normalized_query: "fallback".to_string(),
            include_tests: false,
        },
        policy(SearchShape::Identifier),
        lanes,
    )
    .expect("fallback builder");
    let request =
        parse_public_page_request(&json!({"offset": 0, "topK": 1})).expect("fallback page request");
    let page = serve_public_page(&builder, request).expect("fallback page");
    (page, token)
}

fn telemetry_run(mode: &ModeFixture, token: GenerationToken) -> TelemetryRun {
    TelemetryRun {
        shape: SearchShape::from_str(&mode.shape).expect("known fixture shape"),
        confidence: Some(match mode.confidence.as_str() {
            "high" => ConfidenceTelemetry::High,
            "low" => ConfidenceTelemetry::Low,
            other => panic!("unknown fixture confidence {other}"),
        }),
        variants: mode.variants.clone(),
        embedding_calls: mode.embedding_calls,
        snapshot_generation: token,
    }
}

fn assemble(page: &SearchPage, mode: &ModeFixture, token: GenerationToken) -> StructuredContent {
    let provenance = ObservedProvenance::from_reply(&page.reply).expect("observed provenance");
    TelemetryAssembler::new(page, provenance)
        .assemble(telemetry_run(mode, token))
        .expect("structured telemetry")
}

fn assert_plan_fields(value: &Value, mode: &ModeFixture, lanes: &[&str], exact_tier: &str) {
    let plan = value["plan"].as_object().expect("plan object");
    assert_eq!(
        plan.len(),
        11,
        "the plan field census must remain exhaustive"
    );
    assert_eq!(plan["shape"], mode.shape);
    assert_eq!(plan["lanes_run"], json!(lanes));
    assert_eq!(plan["exact_tier"], exact_tier);
    assert_eq!(plan["confidence"], mode.confidence);
    assert_eq!(plan["variants"], json!(&mode.variants));
    assert_eq!(plan["embedding_calls"], mode.embedding_calls);
    assert_eq!(plan["snapshot_generation"], mode.snapshot_generation);
    assert_eq!(plan["retrieval_depth"], mode.retrieval_depth);
    assert_eq!(plan["depth_tier"], mode.depth_tier);
    assert_eq!(plan["lanes_exhausted"], mode.lanes_exhausted);
    assert_eq!(plan["stability_void"], mode.stability_void);
    for forbidden in ["epoch", "poisoned", "process_nonce", "nonce"] {
        assert!(!plan.contains_key(forbidden), "plan leaked {forbidden}");
    }
}

#[test]
fn ready_mode_structured_content_is_asserted_field_by_field() {
    let fixture = fixture();
    let (page, token) = ready_page();
    let content = assemble(&page, &fixture.ready, token);
    let value = serde_json::to_value(content).expect("serialize ready telemetry");

    assert_plan_fields(
        &value,
        &fixture.ready,
        &["exact", "lexical", "semantic"],
        "e1",
    );
    let results = value["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["file"], "ready-exact.rs");
    assert_eq!(results[0]["r3_order_index"], 0);
    assert!(results[0].get("fusion_score").is_none());
    assert!(results[0].get("lane_score").is_none());
    assert_eq!(
        results[0]["lane_positions"]["exact"],
        json!({"position": 0, "disposition": "depth_exempt"})
    );
    assert_eq!(
        results[0]["lane_positions"]["lexical"],
        json!({"position": 0, "disposition": "provenance_only"})
    );
    assert_eq!(
        results[0]["evidence_descriptor"],
        json!({
            "tier": "exact",
            "kind": "e1",
            "occurrences": 2,
            "matched_span": null,
            "gap_chars": null,
            "window_lines": null,
            "exact_form": true,
            "generated": false
        })
    );

    assert_eq!(results[1]["file"], "ready-non-exact.rs");
    assert!(results[1]["fusion_score"].is_number());
    assert!(results[1]["lane_score"].is_number());
    assert_eq!(
        results[1]["lane_positions"]["lexical"],
        json!({"position": 1, "admitted": true})
    );
    assert_eq!(
        results[1]["lane_positions"]["semantic"],
        json!({"position": 0, "admitted": true})
    );
    assert_eq!(
        results[1]["evidence_descriptor"],
        json!({
            "tier": "non_exact",
            "kind": "none",
            "occurrences": null,
            "matched_span": null,
            "gap_chars": null,
            "window_lines": null,
            "exact_form": true,
            "generated": false
        })
    );
}

#[test]
fn fallback_mode_structured_content_is_asserted_field_by_field() {
    let fixture = fixture();
    let (page, token) = fallback_page();
    let content = assemble(&page, &fixture.fallback, token);
    let value = serde_json::to_value(content).expect("serialize fallback telemetry");

    assert_plan_fields(&value, &fixture.fallback, &["exact", "lexical"], "none");
    let result = &value["results"][0];
    assert_eq!(result["file"], "fallback.rs");
    assert_eq!(result["r3_order_index"], 0);
    assert!(result["fusion_score"].is_number());
    assert!(result["lane_score"].is_number());
    assert_eq!(
        result["lane_positions"],
        json!({"lexical": {"position": 0, "admitted": true}})
    );
    assert_eq!(result["evidence_descriptor"]["tier"], "non_exact");
    assert_eq!(result["evidence_descriptor"]["kind"], "none");
}

struct PanickingLanePositions;

impl LanePositionsAccessor for PanickingLanePositions {
    fn lane_positions(
        &self,
        _entry: &blocks::BlockEntry,
    ) -> Result<LanePositions, ProvenanceError> {
        panic!("lane_positions accessor must remain unreadable during decision stages")
    }
}

fn fixture_confidence(entries: &[blocks::BlockEntry]) -> ConfidenceTelemetry {
    match entries {
        [first, second, ..]
            if first.result.evidence.tier == evidence_descriptor::EvidenceTier::Exact
                && second.result.evidence.tier == evidence_descriptor::EvidenceTier::NonExact =>
        {
            ConfidenceTelemetry::High
        }
        [only] if only.result.evidence.tier == evidence_descriptor::EvidenceTier::Exact => {
            ConfidenceTelemetry::High
        }
        _ => ConfidenceTelemetry::Low,
    }
}

fn assert_decision_fixture(page: &SearchPage, mode: &ModeFixture, exact_pass: ExactPassState<'_>) {
    let assembler = TelemetryAssembler::new(page, PanickingLanePositions);
    let decisions = assembler.decision_view();

    let mut ranked = decisions
        .page_entries()
        .iter()
        .rev()
        .map(|entry| entry.result.clone())
        .collect::<Vec<_>>();
    sort_r3(&mut ranked);
    assert_eq!(
        ranked
            .iter()
            .map(|result| result.path.display().to_string())
            .collect::<Vec<_>>(),
        mode.stability_files
    );

    let confidence = decisions.derive_confidence(fixture_confidence);
    assert_eq!(
        confidence,
        match mode.confidence.as_str() {
            "high" => ConfidenceTelemetry::High,
            "low" => ConfidenceTelemetry::Low,
            _ => unreachable!(),
        }
    );

    let assembled_units = decisions.assemble_page(|entries| {
        entries
            .iter()
            .map(blocks::BlockEntry::stability_unit)
            .collect::<Vec<_>>()
    });
    assert_eq!(assembled_units, decisions.page_stability_units());
    assert_eq!(
        assembled_units
            .iter()
            .map(|unit| unit.ranked_tuple.file.display().to_string())
            .collect::<Vec<_>>(),
        mode.stability_files
    );

    let trailer = decisions
        .assemble_trailer(exact_pass)
        .expect("fixture trailer");
    assert_eq!(
        aft::list_envelope::render_trailer(&trailer.shared_envelope_projection())
            .expect("fixture envelope has a reason"),
        mode.trailer
    );
}

#[test]
fn ranking_confidence_trailer_and_page_assembly_never_read_stubbed_provenance() {
    let fixture = fixture();
    let (ready, _) = ready_page();
    assert_decision_fixture(&ready, &fixture.ready, ExactPassState::Complete);

    let (fallback, _) = fallback_page();
    let bounded_line = fixture
        .fallback
        .bounded_disclosure
        .as_deref()
        .expect("fallback disclosure");
    assert_decision_fixture(
        &fallback,
        &fixture.fallback,
        ExactPassState::Bounded {
            files: 12,
            reason: "file limit",
            disclosure_lines: &[bounded_line],
        },
    );
}

#[test]
fn stability_void_is_emitted_without_memo_or_token_internals() {
    let fixture = fixture();
    let (mut page, token) = ready_page();
    page.stability_void = true;
    let mut mode = fixture.ready;
    mode.stability_void = true;
    let value = serde_json::to_value(assemble(&page, &mode, token)).expect("serialize");
    let plan = value["plan"].as_object().expect("plan object");

    assert_eq!(plan["stability_void"], true);
    assert_eq!(plan["snapshot_generation"], mode.snapshot_generation);
    assert!(!plan.contains_key("epoch"));
    assert!(!plan.contains_key("poisoned"));
    assert!(!plan.contains_key("process_nonce"));
}
