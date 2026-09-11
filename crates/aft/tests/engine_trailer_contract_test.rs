use std::path::PathBuf;

use aft::list_envelope::{render_trailer, Reason};
use aft::list_surfaces::find_surface;

use aft::commands::semantic_search::{
    blocks, evidence_descriptor, paging, plan_table, scoring, trailer,
};

use blocks::{BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate};
use evidence_descriptor::EvidenceDescriptor;
use paging::{
    parse_public_page_request, select_stop_state, serve_public_page, SearchPage, StopConditions,
    StopState,
};
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use scoring::ScoringPolicy;
use trailer::{ExactPassState, SearchTotal, SearchTrailer};

fn candidates(count: usize) -> Vec<LaneCandidate> {
    (0..count)
        .map(|position| {
            LaneCandidate::non_exact(
                format!("candidate-{position:04}.rs"),
                None,
                EvidenceDescriptor::for_non_exact(true, false),
                1.0 - position as f32 / 10_000.0,
                false,
            )
        })
        .collect()
}

fn page(label: &str, candidate_count: usize, offset: usize, top_k: usize) -> SearchPage {
    let lane = CanonicalLane::new(SearchLaneKind::Semantic, candidates(candidate_count))
        .expect("canonical lane");
    let key = CanonicalListKey {
        project_root: PathBuf::from(format!("/virtual/{label}")),
        snapshot_generation: "trailer-generation-1".to_string(),
        normalized_query: label.to_string(),
        include_tests: false,
    };
    let policy =
        ScoringPolicy::from_plan_table(&PlanTable::running_table(), SearchShape::NaturalLanguage)
            .expect("running scoring policy");
    let builder = BlockBuilder::new(key, policy, vec![lane]).expect("block builder");
    let request = parse_public_page_request(&serde_json::json!({
        "offset": offset,
        "topK": top_k
    }))
    .expect("valid request");
    serve_public_page(&builder, request).expect("served page")
}

fn render_shared(trailer: &SearchTrailer) -> String {
    render_trailer(&trailer.shared_envelope_projection())
        .expect("search stop always has a shared envelope reason")
}

#[test]
fn stop_precedence_is_total_for_every_boolean_combination() {
    for interval_satisfied in [false, true] {
        for lanes_exhausted in [false, true] {
            for at_depth_cap in [false, true] {
                let conditions = StopConditions {
                    interval_satisfied,
                    lanes_exhausted,
                    at_depth_cap,
                };
                let selected = select_stop_state(conditions);
                match (lanes_exhausted, at_depth_cap, interval_satisfied) {
                    (true, _, _) => assert_eq!(selected.unwrap(), StopState::S2Exhausted),
                    (false, true, _) => assert_eq!(selected.unwrap(), StopState::S3DepthCap),
                    (false, false, true) => {
                        assert_eq!(selected.unwrap(), StopState::S1MoreAtDepth)
                    }
                    (false, false, false) => assert!(selected.is_err()),
                }
            }
        }
    }
}

#[test]
fn named_stop_forms_use_exhaustion_then_cap_then_more_precedence() {
    let ordinary =
        SearchTrailer::from_page(&page("ordinary", 201, 0, 10), ExactPassState::Complete)
            .expect("ordinary trailer");
    assert_eq!(ordinary.stop_state, StopState::S1MoreAtDepth);
    assert_eq!(ordinary.total, SearchTotal::AtLeast(200));
    assert_eq!(
        render_shared(&ordinary),
        "shown 10 of ≥200 results (cap) · narrow: offset, topK, path, includeTests"
    );

    let exhausted =
        SearchTrailer::from_page(&page("exhausted", 8, 20, 10), ExactPassState::Complete)
            .expect("exhausted trailer");
    assert_eq!(exhausted.stop_state, StopState::S2Exhausted);
    assert_eq!(exhausted.total, SearchTotal::Exact(8));
    assert_eq!(
        render_shared(&exhausted),
        "shown 0 of 8 results (walk) · narrow: offset, topK, path, includeTests"
    );

    let depth_cap =
        SearchTrailer::from_page(&page("depth-cap", 3201, 3200, 10), ExactPassState::Complete)
            .expect("depth-cap trailer");
    assert_eq!(depth_cap.stop_state, StopState::S3DepthCap);
    assert_eq!(depth_cap.total, SearchTotal::AtLeast(3200));
    assert_eq!(
        render_shared(&depth_cap),
        "shown 0 of ≥3200 results (depth) · narrow: offset, topK, path, includeTests"
    );
}

#[test]
fn coincidence_forms_print_exactly_one_agreeing_reason() {
    let fixtures = [
        (
            page("c1-cap-wins-over-satisfied", 3201, 3100, 10),
            StopState::S3DepthCap,
            "shown 10 of ≥3200 results (depth) · narrow: offset, topK, path, includeTests",
        ),
        (
            page("c2-exhaustion-wins-over-satisfied", 210, 200, 10),
            StopState::S2Exhausted,
            "shown 10 of 210 results (walk) · narrow: offset, topK, path, includeTests",
        ),
        (
            page("c3-exhaustion-wins-over-cap", 3200, 3190, 10),
            StopState::S2Exhausted,
            "shown 10 of 3200 results (walk) · narrow: offset, topK, path, includeTests",
        ),
    ];

    for (page, expected_state, expected_text) in fixtures {
        let trailer =
            SearchTrailer::from_page(&page, ExactPassState::Complete).expect("coincidence trailer");
        assert_eq!(trailer.stop_state, expected_state);
        let rendered = render_shared(&trailer);
        assert_eq!(rendered, expected_text);
        assert_eq!(
            ["(walk)", "(depth)", "(cap)"]
                .into_iter()
                .filter(|reason| rendered.contains(reason))
                .count(),
            1
        );
        assert_eq!(
            page.reply.lanes_exhausted,
            expected_state == StopState::S2Exhausted
        );
        if expected_state == StopState::S3DepthCap {
            assert_eq!(page.reply.retrieval_depth, 3200);
        }
    }
}

#[test]
fn exhausted_bounded_exact_pass_requires_its_disclosure_in_the_same_reply() {
    let exhausted_page = page("bounded-exact", 8, 20, 10);
    let missing = SearchTrailer::from_page(
        &exhausted_page,
        ExactPassState::Bounded {
            files: 1000,
            reason: "file limit",
            disclosure_lines: &[],
        },
    )
    .expect_err("bounded exhausted reply without disclosure must fail");
    assert!(missing
        .to_string()
        .contains("exact pass: bounded (1000 files, file limit)"));

    let lines = ["exact pass: bounded (1000 files, file limit)"];
    let disclosed = SearchTrailer::from_page(
        &exhausted_page,
        ExactPassState::Bounded {
            files: 1000,
            reason: "file limit",
            disclosure_lines: &lines,
        },
    )
    .expect("bounded exhausted reply with disclosure");
    assert_eq!(disclosed.stop_state, StopState::S2Exhausted);
}

#[test]
fn shared_projection_is_the_only_trailer_grammar() {
    for (trailer, expected) in [
        (
            SearchTrailer {
                shown: 10,
                total: SearchTotal::AtLeast(200),
                stop_state: StopState::S1MoreAtDepth,
            },
            "shown 10 of ≥200 results (cap) · narrow: offset, topK, path, includeTests",
        ),
        (
            SearchTrailer {
                shown: 0,
                total: SearchTotal::Exact(8),
                stop_state: StopState::S2Exhausted,
            },
            "shown 0 of 8 results (walk) · narrow: offset, topK, path, includeTests",
        ),
        (
            SearchTrailer {
                shown: 0,
                total: SearchTotal::AtLeast(400),
                stop_state: StopState::S3DepthCap,
            },
            "shown 0 of ≥400 results (depth) · narrow: offset, topK, path, includeTests",
        ),
    ] {
        let rendered = render_shared(&trailer);
        assert_eq!(rendered, expected);
        assert_eq!(rendered.matches("shown ").count(), 1);
    }
}

#[test]
fn shared_projection_serializes_reason_and_total_for_every_stop_state() {
    for (trailer, expected_reason, expected_total) in [
        (
            SearchTrailer {
                shown: 10,
                total: SearchTotal::AtLeast(200),
                stop_state: StopState::S1MoreAtDepth,
            },
            "cap",
            serde_json::json!({"kind": "at_least", "value": 200}),
        ),
        (
            SearchTrailer {
                shown: 8,
                total: SearchTotal::Exact(8),
                stop_state: StopState::S2Exhausted,
            },
            "walk",
            serde_json::json!({"kind": "exact", "value": 8}),
        ),
        (
            SearchTrailer {
                shown: 10,
                total: SearchTotal::AtLeast(3200),
                stop_state: StopState::S3DepthCap,
            },
            "depth",
            serde_json::json!({"kind": "at_least", "value": 3200}),
        ),
    ] {
        let envelope = serde_json::to_value(trailer.shared_envelope_projection())
            .expect("serialize shared envelope");
        assert_eq!(envelope["reason"], expected_reason);
        assert_eq!(envelope["total"], expected_total);
        assert_eq!(
            envelope["narrow"],
            serde_json::json!(["offset", "topK", "path", "includeTests"])
        );
        let reason: Reason = serde_json::from_value(envelope["reason"].clone())
            .expect("deserialize projected reason");
        let surface =
            find_surface("search", "", "payload.results").expect("search surface is registered");
        assert!(
            surface.reasons.iter().any(|entry| entry.reason == reason),
            "projected reason {reason:?} must be declared by the search surface"
        );
    }
}
