use std::path::PathBuf;

use aft::list_envelope::render_trailer;

mod comparator {
    pub use aft::commands::semantic_search::comparator::*;
}
mod evidence_descriptor {
    pub use aft::commands::semantic_search::evidence_descriptor::*;
}
mod list_envelope {
    pub use aft::list_envelope::*;
}
mod plan_table {
    pub use aft::commands::semantic_search::plan_table::*;
}

#[allow(dead_code)]
#[path = "../src/commands/semantic_search/blocks.rs"]
mod blocks;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/paging.rs"]
mod paging;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/scoring.rs"]
mod scoring;
#[allow(dead_code)]
#[path = "../src/commands/semantic_search/trailer.rs"]
mod trailer;

use blocks::{BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate};
use evidence_descriptor::EvidenceDescriptor;
use paging::{
    parse_public_page_request, select_stop_state, serve_public_page, SearchPage, StopConditions,
    StopState,
};
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use scoring::ScoringPolicy;
use trailer::{stop_reason_word, ExactPassState, SearchTotal, SearchTrailer};

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

fn render_with_search_reason(trailer: &SearchTrailer) -> String {
    let shared = render_trailer(&trailer.shared_envelope_projection())
        .expect("search stop always has a shared envelope reason");
    let shared_reason = match trailer.stop_state {
        StopState::S1MoreAtDepth => "cap",
        StopState::S2Exhausted => "walk",
        StopState::S3DepthCap => "depth",
    };
    let mut rendered = shared.replace(
        &format!("({shared_reason})"),
        &format!("({})", stop_reason_word(trailer.stop_state)),
    );
    if let SearchTotal::AtLeast(value) = trailer.total {
        rendered = rendered.replace(&format!("≥{value}"), &format!("{value}+"));
    }
    rendered
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
        render_with_search_reason(&ordinary),
        "shown 10 of 200+ results (more at greater depth) · narrow: offset, topK, path, includeTests"
    );

    let exhausted =
        SearchTrailer::from_page(&page("exhausted", 8, 20, 10), ExactPassState::Complete)
            .expect("exhausted trailer");
    assert_eq!(exhausted.stop_state, StopState::S2Exhausted);
    assert_eq!(exhausted.total, SearchTotal::Exact(8));
    assert_eq!(
        render_with_search_reason(&exhausted),
        "shown 0 of 8 results (exhausted) · narrow: offset, topK, path, includeTests"
    );

    let depth_cap =
        SearchTrailer::from_page(&page("depth-cap", 3201, 3200, 10), ExactPassState::Complete)
            .expect("depth-cap trailer");
    assert_eq!(depth_cap.stop_state, StopState::S3DepthCap);
    assert_eq!(depth_cap.total, SearchTotal::AtLeast(3200));
    assert_eq!(
        render_with_search_reason(&depth_cap),
        "shown 0 of 3200+ results (depth cap) · narrow: offset, topK, path, includeTests"
    );
}

#[test]
fn coincidence_forms_print_exactly_one_agreeing_reason() {
    let fixtures = [
        (
            page("c1-cap-wins-over-satisfied", 3201, 3100, 10),
            StopState::S3DepthCap,
            "shown 10 of 3200+ results (depth cap) · narrow: offset, topK, path, includeTests",
        ),
        (
            page("c2-exhaustion-wins-over-satisfied", 210, 200, 10),
            StopState::S2Exhausted,
            "shown 10 of 210 results (exhausted) · narrow: offset, topK, path, includeTests",
        ),
        (
            page("c3-exhaustion-wins-over-cap", 3200, 3190, 10),
            StopState::S2Exhausted,
            "shown 10 of 3200 results (exhausted) · narrow: offset, topK, path, includeTests",
        ),
    ];

    for (page, expected_state, expected_text) in fixtures {
        let trailer =
            SearchTrailer::from_page(&page, ExactPassState::Complete).expect("coincidence trailer");
        assert_eq!(trailer.stop_state, expected_state);
        let rendered = render_with_search_reason(&trailer);
        assert_eq!(rendered, expected_text);
        assert_eq!(
            ["(exhausted)", "(depth cap)", "(more at greater depth)"]
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
fn shared_renderer_parity_differs_only_by_search_reason_and_total_notation() {
    for trailer in [
        SearchTrailer {
            shown: 10,
            total: SearchTotal::AtLeast(200),
            stop_state: StopState::S1MoreAtDepth,
        },
        SearchTrailer {
            shown: 0,
            total: SearchTotal::Exact(8),
            stop_state: StopState::S2Exhausted,
        },
        SearchTrailer {
            shown: 0,
            total: SearchTotal::AtLeast(400),
            stop_state: StopState::S3DepthCap,
        },
    ] {
        let shared = render_trailer(&trailer.shared_envelope_projection()).unwrap();
        let search = render_with_search_reason(&trailer);
        assert_eq!(
            shared.matches("shown ").count(),
            1,
            "shared renderer owns the trailer grammar"
        );
        assert_eq!(search.matches("shown ").count(), 1);
        assert!(search.ends_with(" · narrow: offset, topK, path, includeTests"));
        assert!(search.contains(stop_reason_word(trailer.stop_state)));
        if trailer.stop_state == StopState::S3DepthCap {
            assert_eq!(
                search,
                "shown 0 of 400+ results (depth cap) · narrow: offset, topK, path, includeTests"
            );
        }
        assert_eq!(
            trailer.total.campaign_suffix(),
            if matches!(trailer.total, SearchTotal::Exact(_)) {
                ""
            } else {
                "+"
            }
        );
    }
}
