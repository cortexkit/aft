use aft::list_envelope::{Reason, Unit};
use aft::list_surfaces::{ReasonKind, LIST_SURFACES};

#[derive(Debug)]
struct TableTestCase {
    surface_id: &'static str,
    causes: &'static [Reason],
    expected_selected_reason: Reason,
    expected_causes_order: &'static [Reason],
    expected_is_at_least: bool,
    expected_unit: Unit,
}

#[test]
fn kind_bound_table_test_for_registered_surfaces() {
    let test_cases = [
        // Callgraph impact: cap only -> Exact, Selecting, unit sites
        TableTestCase {
            surface_id: "payload.sites",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false, // Selecting with post-filter count -> Exact
            expected_unit: Unit::Sites,
        },
        // Callgraph impact: depth only -> AtLeast, Bounding
        TableTestCase {
            surface_id: "payload.sites",
            causes: &[Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth],
            expected_is_at_least: true, // Bounding -> AtLeast
            expected_unit: Unit::Sites,
        },
        // Callgraph impact: cap + depth -> Depth > Cap, AtLeast
        TableTestCase {
            surface_id: "payload.sites",
            causes: &[Reason::Cap, Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth, Reason::Cap],
            expected_is_at_least: true,
            expected_unit: Unit::Sites,
        },
        TableTestCase {
            surface_id: "payload.sites",
            causes: &[Reason::Budget],
            expected_selected_reason: Reason::Budget,
            expected_causes_order: &[Reason::Budget],
            expected_is_at_least: true,
            expected_unit: Unit::Sites,
        },
        TableTestCase {
            surface_id: "payload.sites",
            causes: &[Reason::Cap, Reason::Budget, Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth, Reason::Budget, Reason::Cap],
            expected_is_at_least: true,
            expected_unit: Unit::Sites,
        },
        // Callers: cap only -> Exact, Selecting, unit items
        TableTestCase {
            surface_id: "payload.callers",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false,
            expected_unit: Unit::Items,
        },
        // Callers: depth cut inside requested -> Depth, AtLeast, unit items
        TableTestCase {
            surface_id: "payload.callers",
            causes: &[Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth],
            expected_is_at_least: true,
            expected_unit: Unit::Items,
        },
        // Callers: macro-template entry limit cut -> Budget, AtLeast, unit items
        TableTestCase {
            surface_id: "payload.callers",
            causes: &[Reason::Budget],
            expected_selected_reason: Reason::Budget,
            expected_causes_order: &[Reason::Budget],
            expected_is_at_least: true,
            expected_unit: Unit::Items,
        },
        // Callers: macro cut with depth and hub cap -> Depth > Budget > Cap, AtLeast
        TableTestCase {
            surface_id: "payload.callers",
            causes: &[Reason::Cap, Reason::Budget, Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth, Reason::Budget, Reason::Cap],
            expected_is_at_least: true,
            expected_unit: Unit::Items,
        },
        // Call tree: cap only -> Exact, Selecting, unit items
        TableTestCase {
            surface_id: "payload.tree",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false,
            expected_unit: Unit::Items,
        },
        // Trace to: depth only -> Depth, AtLeast, unit paths
        TableTestCase {
            surface_id: "payload.paths",
            causes: &[Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth],
            expected_is_at_least: true,
            expected_unit: Unit::Paths,
        },
        // Trace to: budget only -> Budget, AtLeast, unit paths
        TableTestCase {
            surface_id: "payload.paths",
            causes: &[Reason::Budget],
            expected_selected_reason: Reason::Budget,
            expected_causes_order: &[Reason::Budget],
            expected_is_at_least: true,
            expected_unit: Unit::Paths,
        },
        // Trace to: cap only (retained path limit R26) -> Cap, Exact, unit paths
        TableTestCase {
            surface_id: "payload.paths",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false,
            expected_unit: Unit::Paths,
        },
        // Trace data: depth only -> Depth, AtLeast, unit hops
        TableTestCase {
            surface_id: "payload.hops",
            causes: &[Reason::Depth],
            expected_selected_reason: Reason::Depth,
            expected_causes_order: &[Reason::Depth],
            expected_is_at_least: true,
            expected_unit: Unit::Hops,
        },
        // Search: cap only (more_available R8/R16) -> Cap, AtLeast(shown+1), Selecting with floor
        TableTestCase {
            surface_id: "payload.results",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: true, // floor only -> AtLeast per R19
            expected_unit: Unit::Results,
        },
        // Search: budget only (engine_capped) -> Budget, AtLeast(shown), Bounding
        TableTestCase {
            surface_id: "payload.results",
            causes: &[Reason::Budget],
            expected_selected_reason: Reason::Budget,
            expected_causes_order: &[Reason::Budget],
            expected_is_at_least: true,
            expected_unit: Unit::Results,
        },
        // Search: both flags -> Budget > Cap, AtLeast(shown+1), causes: ["budget", "cap"]
        TableTestCase {
            surface_id: "payload.results",
            causes: &[Reason::Cap, Reason::Budget],
            expected_selected_reason: Reason::Budget,
            expected_causes_order: &[Reason::Budget, Reason::Cap],
            expected_is_at_least: true,
            expected_unit: Unit::Results,
        },
        // Grep: cap only -> Cap, AtLeast (when at cap floor) or Exact (when display cap only)
        TableTestCase {
            surface_id: "payload.matches",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: true, // executor cap floor
            expected_unit: Unit::Rows,
        },
        // Grep: walk only -> Walk, AtLeast, unit rows
        TableTestCase {
            surface_id: "payload.matches",
            causes: &[Reason::Walk],
            expected_selected_reason: Reason::Walk,
            expected_causes_order: &[Reason::Walk],
            expected_is_at_least: true,
            expected_unit: Unit::Rows,
        },
        // Glob: cap only -> Cap, unit files
        TableTestCase {
            surface_id: "payload.files",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false, // display cap under executor cap -> Exact
            expected_unit: Unit::Files,
        },
        // Outline files: budget rollups present -> Budget, Selecting, Exact(shown + rollups), unit files
        TableTestCase {
            surface_id: "payload.files",
            causes: &[Reason::Budget],
            expected_selected_reason: Reason::Budget,
            expected_causes_order: &[Reason::Budget],
            expected_is_at_least: false, // counted post-filter domain size
            expected_unit: Unit::Files,
        },
        // Outline files: collection_truncated -> Walk, Bounding, AtLeast, unit files
        TableTestCase {
            surface_id: "payload.files",
            causes: &[Reason::Walk],
            expected_selected_reason: Reason::Walk,
            expected_causes_order: &[Reason::Walk],
            expected_is_at_least: true,
            expected_unit: Unit::Files,
        },
        // Inspect: cap only -> Cap, Selecting, unit items
        TableTestCase {
            surface_id: "payload.details",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false,
            expected_unit: Unit::Items,
        },
        // Bash: cap only -> Cap, Selecting, Exact(input line count), unit lines
        TableTestCase {
            surface_id: "bash.output",
            causes: &[Reason::Cap],
            expected_selected_reason: Reason::Cap,
            expected_causes_order: &[Reason::Cap],
            expected_is_at_least: false,
            expected_unit: Unit::Lines,
        },
    ];

    for tc in test_cases {
        let mut causes = tc.causes.to_vec();
        causes.sort_by(|a, b| b.precedence().cmp(&a.precedence()));
        assert_eq!(
            causes[0], tc.expected_selected_reason,
            "selected reason mismatch for {}",
            tc.surface_id
        );
        assert_eq!(
            causes.as_slice(),
            tc.expected_causes_order,
            "causes order mismatch for {}",
            tc.surface_id
        );

        let surface = LIST_SURFACES
            .iter()
            .find(|s| s.list_id == tc.surface_id)
            .expect("surface exists");
        assert_eq!(
            surface.unit, tc.expected_unit,
            "unit mismatch for {}",
            tc.surface_id
        );

        // Verify that Bounding cause implies at_least
        let has_bounding = surface
            .reasons
            .iter()
            .any(|r| tc.causes.contains(&r.reason) && r.kind == ReasonKind::Bounding);
        if has_bounding {
            assert!(
                tc.expected_is_at_least,
                "Bounding cause on {} must produce at_least",
                tc.surface_id
            );
        }
    }
}
