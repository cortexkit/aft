#[path = "list_envelope_conformance/support.rs"]
mod support;

use aft::commands::callgraph_store_adapter::callgraph_surface::HUB_SUMMARY_LIMIT;
use aft::list_envelope::{Reason, Total, Unit};
use aft::list_surfaces::ReasonKind;
use serde_json::json;
use support::{
    capped_fixtures, capped_parity_fixtures, discover_list_cutting_sites, find_call_sites,
    kind_bound_cases, validate_capped_parity_coverage, validate_discovered_cuts,
    validate_exemption_list, validate_fixture_schema, validate_hub_trigger,
    validate_kind_bound_case, validate_recursive_envelope_keys, validate_renderer_call_sites,
    validate_surface_specs, validate_trailer, CallSite, CompletePathExemption, DiscoveredCut,
    ReasonSpec, COMPLETE_PATH_EXEMPTIONS, SURFACE_SPECS,
};

const MUTATED_SEARCH_REASONS: &[ReasonSpec] = &[
    ReasonSpec {
        reason: Reason::Budget,
        kind: ReasonKind::Selecting,
    },
    ReasonSpec {
        reason: Reason::Cap,
        kind: ReasonKind::Selecting,
    },
];

#[test]
fn removing_one_surfaces_envelope_fails_the_schema_assertion() {
    let mut fixture = capped_fixtures()
        .into_iter()
        .find(|fixture| fixture.name == "product/search_attachment")
        .expect("product search attachment fixture");
    fixture
        .reply
        .as_object_mut()
        .expect("search reply object")
        .remove("results_list_envelope");

    let errors = validate_fixture_schema(&fixture);
    assert_eq!(
        errors,
        ["product/search_attachment: missing capped envelope key results_list_envelope"]
    );
}

#[test]
fn flipping_one_reason_kind_fails_the_registry_assertion() {
    let mut observed = SURFACE_SPECS.to_vec();
    let search = observed
        .iter_mut()
        .find(|surface| surface.command == "search")
        .expect("search surface");
    search.reasons = MUTATED_SEARCH_REASONS;

    let errors = validate_surface_specs(&observed);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("search: surface reason kinds expected"));
    assert!(errors[0].contains("got"));
}

#[test]
fn flipping_one_bound_fails_the_table_assertion() {
    let mut case = kind_bound_cases()
        .into_iter()
        .find(|case| case.name == "search:cap")
        .expect("search cap-only row");
    case.envelope.as_mut().expect("capped envelope").total = Total::Exact(11);

    assert_eq!(
        validate_kind_bound_case(&case),
        ["search:cap: total expected AtLeast(11), got Exact(11)"]
    );
}

#[test]
fn swapping_a_registered_unit_for_a_sibling_fails_the_registry_assertion() {
    let mut observed = SURFACE_SPECS.to_vec();
    let impact = observed
        .iter_mut()
        .find(|surface| surface.mode == "impact")
        .expect("impact surface");
    impact.unit = Unit::Items;

    assert_eq!(
        validate_surface_specs(&observed),
        ["callgraph.impact: surface unit expected Sites, got Items"]
    );
}

#[test]
fn deriving_the_hub_trigger_from_the_retained_limit_fails_the_boundary_assertion() {
    let errors = validate_hub_trigger(|count| count > HUB_SUMMARY_LIMIT);
    assert_eq!(errors.len(), 2);
    assert!(errors[0].contains("hub trigger at 16: expected false, got true"));
    assert!(errors[1].contains("hub trigger at 20: expected false, got true"));
}

#[test]
fn adding_a_third_exemption_fails_the_closed_set_assertion() {
    let mut exemptions = COMPLETE_PATH_EXEMPTIONS.to_vec();
    exemptions.push(CompletePathExemption::SyntheticThirdEntry);

    let errors = validate_exemption_list(&exemptions);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("complete-path exemption set must be exactly"));
    assert!(errors[0].contains("SyntheticThirdEntry"));
}

#[test]
fn changing_trailer_wording_fails_every_capped_fixture_assertion() {
    let mut fixtures = capped_fixtures();
    let fixture_count = fixtures.len();
    for fixture in &mut fixtures {
        fixture.rendered = fixture.rendered.replace("shown ", "displayed ");
    }

    let failures = fixtures
        .iter()
        .filter(|fixture| !validate_trailer(fixture).is_empty())
        .map(|fixture| fixture.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), fixture_count);
    assert!(failures.contains(&"search/both_flags_mixed".to_string()));
    assert!(failures.contains(&"bash/capped_4000_in_61_out".to_string()));
    assert!(failures.contains(&"table/callgraph.impact:cap".to_string()));
}

#[test]
fn adding_a_fourth_renderer_caller_fails_while_measurement_still_passes() {
    let mut render_sites = find_call_sites("render_trailer");
    let measure_sites = find_call_sites("measure_trailer_len");
    assert!(validate_renderer_call_sites(&render_sites, &measure_sites).is_empty());

    render_sites.push(CallSite {
        file: "commands/synthetic.rs".to_string(),
        line: 1,
        code: "render_trailer(envelope)".to_string(),
    });
    let errors = validate_renderer_call_sites(&render_sites, &measure_sites);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("render_trailer callers must be exactly"));
    assert!(!errors[0].contains("measure_trailer_len caller"));
}

#[test]
fn emitting_a_top_level_list_envelope_fails_the_recursive_key_assertion() {
    let mut fixture = capped_fixtures()
        .into_iter()
        .find(|fixture| fixture.name == "product/bash_attachment")
        .expect("product bash attachment fixture");
    fixture.reply["list_envelope"] = json!({
        "shown": 2,
        "total": {"kind": "exact", "value": 5},
        "unit": "lines",
        "reason": "cap",
        "causes": ["cap"],
        "narrow": []
    });

    assert_eq!(
        validate_recursive_envelope_keys(&fixture.reply),
        [": key literally named list_envelope is forbidden"]
    );
}

#[test]
fn adding_an_unregistered_capped_array_fails_registry_free_discovery() {
    let mut discovered = discover_list_cutting_sites();
    discovered.push(DiscoveredCut {
        file: "commands/extract.rs".to_string(),
        line: 123,
        enclosing_item: "unregistered_capped_array".to_string(),
        primitive: "truncate(",
        snippet: "results.truncate(3);".to_string(),
    });

    assert_eq!(
        validate_discovered_cuts(&discovered),
        ["unregistered list-cutting site at commands/extract.rs:123: primitive 'truncate(' in item 'unregistered_capped_array'"]
    );
}

#[test]
fn deleting_one_capped_parity_fixture_fails_the_coverage_guard() {
    let mut fixtures = capped_parity_fixtures();
    let initial_len = fixtures.len();
    fixtures.retain(|f| !(f.command == "callgraph" && f.mode == "callers"));
    assert_eq!(fixtures.len(), initial_len - 1);

    let errors = validate_capped_parity_coverage(&fixtures);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("registered surface has no capped parity fixture"));
    assert!(errors[0].contains("callgraph.callers"));
}
