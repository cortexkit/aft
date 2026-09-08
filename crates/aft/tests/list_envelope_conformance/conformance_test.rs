use std::collections::BTreeSet;

use aft::list_envelope::{Reason, Total, Unit};
use aft::list_surfaces::EXCLUSIONS;

use super::support::{
    capped_fixtures, capped_parity_fixtures, complete_builder_results, complete_fixtures,
    discover_list_cutting_sites, find_call_sites, fixture_pairs, kind_bound_cases,
    registered_pairs, render_complete_transports, render_transports,
    validate_capped_parity_coverage, validate_discovered_cuts, validate_exclusion_reasons,
    validate_exemption_list, validate_fixture_schema, validate_hub_trigger,
    validate_kind_bound_case, validate_no_per_reason_drop_counts, validate_pair_coverage,
    validate_recursive_envelope_keys, validate_registry, validate_renderer_call_sites,
    validate_trailer, CompletePathExemption, COMPLETE_PATH_EXEMPTIONS, SURFACE_SPECS,
};

fn assert_no_errors(context: &str, errors: Vec<String>) {
    assert!(
        errors.is_empty(),
        "{context} failed with {} error(s):\n{}",
        errors.len(),
        errors.join("\n")
    );
}

fn envelope_key_paths(value: &serde_json::Value, prefix: &str, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                if key == "list_envelope" || key.ends_with("_list_envelope") {
                    found.push(path.clone());
                }
                envelope_key_paths(child, &path, found);
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                envelope_key_paths(child, &format!("{prefix}[{index}]"), found);
            }
        }
        _ => {}
    }
}

#[test]
fn registered_cause_sets_have_pinned_kind_bounds_and_units() {
    assert_no_errors("registry conformance", validate_registry());

    let cases = kind_bound_cases();
    let expected_case_count = SURFACE_SPECS
        .iter()
        .map(|surface| (1usize << surface.reasons.len()) - 1)
        .sum::<usize>();
    assert_eq!(
        cases.len(),
        expected_case_count,
        "the table must enumerate every non-empty registered cause set"
    );

    let mut errors = Vec::new();
    for case in &cases {
        errors.extend(validate_kind_bound_case(case));
    }
    errors.extend(validate_hub_trigger(super::support::canonical_hub_trigger));
    assert_no_errors("kind/bound table", errors);

    let search_cap = cases
        .iter()
        .find(|case| case.name == "search:cap")
        .expect("explicit search cap-only row");
    assert_eq!(search_cap.expected_reason, Reason::Cap);
    assert_eq!(search_cap.expected_total, Total::AtLeast(11));
    assert_eq!(search_cap.surface.unit, Unit::Results);

    let search_both = cases
        .iter()
        .find(|case| case.name == "search:budget+cap")
        .expect("explicit search both-flags row");
    assert_eq!(search_both.expected_reason, Reason::Budget);
    assert_eq!(search_both.expected_causes, [Reason::Budget, Reason::Cap]);
    assert_eq!(search_both.expected_total, Total::AtLeast(11));

    let grep_cap = cases
        .iter()
        .find(|case| case.name == "grep:cap")
        .expect("explicit grep cap-only row");
    assert_eq!(grep_cap.expected_reason, Reason::Cap);
    assert_eq!(grep_cap.expected_total, Total::AtLeast(1204));
    assert_eq!(grep_cap.surface.unit, Unit::Rows);
}

#[test]
fn capped_fixtures_have_pinned_wire_schema() {
    let fixtures = capped_fixtures();
    assert!(
        !fixtures.is_empty(),
        "capped fixture catalog must not be empty"
    );

    let mut errors = Vec::new();
    for fixture in &fixtures {
        errors.extend(validate_fixture_schema(fixture));
    }
    assert_no_errors("capped fixture schema", errors);

    let mixed = fixtures
        .iter()
        .find(|fixture| fixture.name == "search/both_flags_mixed")
        .and_then(|fixture| fixture.envelope())
        .expect("pinned search both-flags wire fixture");
    assert_eq!(mixed.shown, 10);
    assert_eq!(mixed.total, Total::AtLeast(11));
    assert_eq!(mixed.unit, Unit::Results);
    assert_eq!(mixed.reason, Some(Reason::Budget));
    assert_eq!(mixed.causes, [Reason::Budget, Reason::Cap]);
    assert_eq!(mixed.narrow, ["topK", "path", "includeTests"]);

    for (name, envelope) in complete_builder_results() {
        assert_eq!(
            envelope, None,
            "{name}: no registered cause fired, so the envelope must be absent"
        );
    }
    for fixture in complete_fixtures() {
        let mut paths = Vec::new();
        envelope_key_paths(&fixture.reply, "", &mut paths);
        assert!(
            paths.is_empty(),
            "{}: complete fixture serialized envelope keys: {:?}",
            fixture.name,
            paths
        );
    }
}

#[test]
fn every_capped_fixture_uses_one_pinned_trailer_and_registered_unit() {
    let fixtures = capped_fixtures();
    let mut errors = Vec::new();
    for fixture in &fixtures {
        errors.extend(validate_trailer(fixture));
    }
    assert_no_errors("shared trailer and unit conformance", errors);

    let impact = fixtures
        .iter()
        .find(|fixture| fixture.name == "table/callgraph.impact:cap")
        .and_then(|fixture| fixture.envelope())
        .expect("impact fixture");
    let callers = fixtures
        .iter()
        .find(|fixture| fixture.name == "table/callgraph.callers:cap")
        .and_then(|fixture| fixture.envelope())
        .expect("callers fixture");
    assert_eq!(impact.unit, Unit::Sites);
    assert_eq!(callers.unit, Unit::Items);
}

#[test]
fn recursive_envelope_keys_are_registered_and_never_bare() {
    let mut errors = Vec::new();
    for fixture in capped_fixtures() {
        errors.extend(
            validate_recursive_envelope_keys(&fixture.reply)
                .into_iter()
                .map(|error| format!("{}: {error}", fixture.name)),
        );
    }
    for fixture in complete_fixtures() {
        errors.extend(
            validate_recursive_envelope_keys(&fixture.reply)
                .into_iter()
                .map(|error| format!("{}: {error}", fixture.name)),
        );
    }
    assert_no_errors("recursive envelope-key inventory", errors);
}

#[test]
fn scoped_subtraction_never_emits_per_reason_drop_counts() {
    let mut errors = Vec::new();
    for fixture in capped_fixtures() {
        errors.extend(
            validate_no_per_reason_drop_counts(&fixture.reply, &fixture.rendered)
                .into_iter()
                .map(|error| format!("{}: {error}", fixture.name)),
        );
        let envelope = fixture
            .envelope()
            .unwrap_or_else(|| panic!("{}: capped envelope", fixture.name));
        assert!(
            envelope.reason.is_some(),
            "{}: reason presence is the envelope completeness signal",
            fixture.name
        );
    }
    assert_no_errors("scoped subtraction", errors);
}

#[test]
fn every_registered_surface_and_reason_has_a_fixture() {
    let cases = kind_bound_cases();
    let produced = fixture_pairs(&cases);
    assert_eq!(produced, registered_pairs());
    assert_no_errors(
        "surface/reason fixture coverage",
        validate_pair_coverage(&produced),
    );

    let constructed_surfaces = cases
        .iter()
        .filter(|case| case.envelope.is_some())
        .map(|case| {
            (
                case.surface.command,
                case.surface.mode,
                case.surface.list_id,
            )
        })
        .collect::<BTreeSet<_>>();
    let registered_surfaces = SURFACE_SPECS
        .iter()
        .map(|surface| (surface.command, surface.mode, surface.list_id))
        .collect::<BTreeSet<_>>();
    assert_eq!(constructed_surfaces, registered_surfaces);
}

#[test]
fn complete_path_exemption_list_is_closed_and_goldens_are_exact() {
    assert_no_errors(
        "complete-path exemptions",
        validate_exemption_list(COMPLETE_PATH_EXEMPTIONS),
    );

    let fixtures = complete_fixtures();
    let represented = fixtures
        .iter()
        .filter_map(|fixture| fixture.exemption)
        .collect::<BTreeSet<_>>();
    let expected = COMPLETE_PATH_EXEMPTIONS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(represented, expected);

    for fixture in fixtures {
        assert_eq!(
            fixture.rendered, fixture.golden,
            "{}: complete-path rendering drifted from its pinned golden",
            fixture.name
        );
        assert!(
            !fixture.rendered.contains("shown "),
            "{}: complete path rendered a trailer",
            fixture.name
        );
        if fixture.exemption == Some(CompletePathExemption::HonoredDepthCallgraphLegacyClause) {
            assert!(!fixture.rendered.contains("(depth limited)"));
        }
        if fixture.exemption == Some(CompletePathExemption::DataHeavyOutlineRollupSentence) {
            assert!(!fixture.rendered.contains("shown as rollups"));
            assert!(!fixture.rendered.contains("shown as a rollup"));
        }
        let (subc_text, ndjson_text) = render_complete_transports(&fixture);
        assert_eq!(
            subc_text, ndjson_text,
            "{}: complete-path transport parity mismatch between subc and ndjson",
            fixture.name
        );
        assert_eq!(
            ndjson_text, fixture.golden,
            "{}: complete-path ndjson rendering drifted from its pinned golden",
            fixture.name
        );
    }
}

#[test]
fn transport_parity_corpus_covers_every_registered_surface_and_is_byte_equal() {
    let fixtures = capped_parity_fixtures();
    assert_no_errors(
        "capped parity coverage",
        validate_capped_parity_coverage(&fixtures),
    );

    for fixture in &fixtures {
        let (subc_text, ndjson_text) = render_transports(fixture);
        assert_eq!(
            subc_text, ndjson_text,
            "{}: transport parity mismatch between subc and ndjson",
            fixture.name
        );
        assert!(
            subc_text.contains("shown "),
            "{}: capped fixture subc output must contain trailer",
            fixture.name
        );
        assert!(
            ndjson_text.contains("shown "),
            "{}: capped fixture ndjson output must contain trailer",
            fixture.name
        );
    }
}

#[test]
fn renderer_and_measurement_callers_are_closed() {
    let render_sites = find_call_sites("render_trailer");
    let measure_sites = find_call_sites("measure_trailer_len");
    assert_no_errors(
        "renderer call-site guard",
        validate_renderer_call_sites(&render_sites, &measure_sites),
    );
}

#[test]
fn registry_free_discovery_resolves_every_cut() {
    let discovered = discover_list_cutting_sites();
    assert!(!discovered.is_empty(), "discovery must find list cuts");
    assert_no_errors(
        "registry-free list-cut discovery",
        validate_discovered_cuts(&discovered),
    );
    assert_no_errors(
        "discovery exclusion reasons",
        validate_exclusion_reasons(EXCLUSIONS),
    );
}
