use aft::commands::trace_to::trace::{
    attach_trace_data_envelope, attach_trace_to_envelope, build_trace_data_envelope,
    build_trace_to_envelope, TRACE_DATA_LIST_ID, TRACE_DATA_WIRE_KEY, TRACE_TO_LIST_ID,
    TRACE_TO_WIRE_KEY,
};
use aft::list_envelope::{derive_wire_key, render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::find_surface;
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::Value;

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/trace/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {path}: {e}"));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("failed to parse fixture JSON {path}: {e}"))
}

fn assert_no_bare_list_envelope(val: &Value) {
    match val {
        Value::Object(map) => {
            assert!(
                !map.contains_key("list_envelope"),
                "found forbidden bare key 'list_envelope' in JSON reply"
            );
            for value in map.values() {
                assert_no_bare_list_envelope(value);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                assert_no_bare_list_envelope(item);
            }
        }
        _ => {}
    }
}

fn extract_trailer_unit(rendered: &str) -> String {
    let parts: Vec<&str> = rendered.split_whitespace().collect();
    // Trailer format: shown <shown> of <total> <unit> (<reason>) · narrow: ...
    // parts[0] = "shown", parts[1] = <shown>, parts[2] = "of", parts[3] = <total>, parts[4] = <unit>
    assert!(
        parts.len() >= 5 && parts[0] == "shown" && parts[2] == "of",
        "invalid trailer structure: {rendered}"
    );
    parts[4].to_string()
}

// ---------------------------------------------------------------------------
// Acceptance Criteria Tests
// ---------------------------------------------------------------------------

#[test]
fn test_depth_exhaustion_no_path_renders_trailer_and_no_dropped_count() {
    let data = load_fixture("depth_exhaustion_no_path.json");
    let resp = Response {
        id: "trace-1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        callgraph_op: Some("trace_to".to_string()),
        ..Default::default()
    };

    let formatted = format_response_with_context("callgraph", &resp, &ctx);
    let expected_trailer = "shown 0 of ≥0 paths (depth) · narrow: depth, includeTests";
    assert!(
        formatted.contains(expected_trailer),
        "rendered text must contain expected trailer:\nexpected: {expected_trailer}\ngot: {formatted}"
    );

    // The fixture asserts no per-reason dropped count exists in text or JSON.
    assert!(
        !formatted.to_lowercase().contains("dropped"),
        "text must not contain any dropped count"
    );
    let json_str = serde_json::to_string(&data).unwrap();
    assert!(
        !json_str.contains("dropped"),
        "JSON must not contain any dropped count field"
    );

    // Verify envelope wire shape and fields
    let env_val = &data["paths_list_envelope"];
    let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(envelope.shown, 0);
    assert_eq!(envelope.total, Total::AtLeast(0));
    assert_eq!(envelope.unit, Unit::Paths);
    assert_eq!(envelope.reason, Some(Reason::Depth));
    assert_eq!(envelope.causes, vec![Reason::Depth]);
    assert_eq!(envelope.narrow, vec!["depth", "includeTests"]);

    assert_no_bare_list_envelope(&data);
}

#[test]
fn test_budget_exhaustion_renders_trailer_with_budget_reason() {
    let data = load_fixture("budget_exhaustion.json");
    let resp = Response {
        id: "trace-2".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        callgraph_op: Some("trace_to".to_string()),
        ..Default::default()
    };

    let formatted = format_response_with_context("callgraph", &resp, &ctx);
    let expected_trailer = "shown 15 of ≥15 paths (budget) · narrow: depth, includeTests";
    assert!(
        formatted.contains(expected_trailer),
        "rendered text must contain expected budget trailer:\nexpected: {expected_trailer}\ngot: {formatted}"
    );

    let env_val = &data["paths_list_envelope"];
    let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(envelope.shown, 15);
    assert_eq!(envelope.total, Total::AtLeast(15));
    assert_eq!(envelope.unit, Unit::Paths);
    assert_eq!(envelope.reason, Some(Reason::Budget));
    assert_eq!(envelope.causes, vec![Reason::Budget]);
    assert_eq!(envelope.narrow, vec!["depth", "includeTests"]);

    assert_no_bare_list_envelope(&data);
}

#[test]
fn test_budget_plus_max_depth_renders_depth_trailer_retains_both_flags() {
    let data = load_fixture("budget_plus_max_depth.json");
    let resp = Response {
        id: "trace-3".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        callgraph_op: Some("trace_to".to_string()),
        ..Default::default()
    };

    let formatted = format_response_with_context("callgraph", &resp, &ctx);
    // Depth has higher precedence than budget: renders (depth) trailer
    let expected_trailer = "shown 15 of ≥15 paths (depth) · narrow: depth, includeTests";
    assert!(
        formatted.contains(expected_trailer),
        "rendered text must contain (depth) trailer:\nexpected: {expected_trailer}\ngot: {formatted}"
    );

    // Both flags retained in JSON
    assert_eq!(data["max_depth_reached"], Value::Bool(true));
    assert_eq!(data["total_paths_is_lower_bound"], Value::Bool(true));

    let env_val = &data["paths_list_envelope"];
    let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(envelope.shown, 15);
    assert_eq!(envelope.total, Total::AtLeast(15));
    assert_eq!(envelope.unit, Unit::Paths);
    assert_eq!(envelope.reason, Some(Reason::Depth));
    assert_eq!(envelope.causes, vec![Reason::Depth, Reason::Budget]);
    assert_eq!(envelope.narrow, vec!["depth", "includeTests"]);

    assert_no_bare_list_envelope(&data);
}

#[test]
fn test_r26_cap_only_renders_exact_cap_trailer_and_mutation_reds() {
    let data = load_fixture("cap_only_r26.json");
    let resp = Response {
        id: "trace-4".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        callgraph_op: Some("trace_to".to_string()),
        ..Default::default()
    };

    let formatted = format_response_with_context("callgraph", &resp, &ctx);
    let expected_trailer = "shown 15 of 60 paths (cap) · narrow: depth, includeTests";
    assert!(
        formatted.contains(expected_trailer),
        "rendered text must contain exact cap trailer:\nexpected: {expected_trailer}\ngot: {formatted}"
    );

    let env_val = &data["paths_list_envelope"];
    let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(envelope.shown, 15);
    assert_eq!(envelope.total, Total::Exact(60));
    assert_eq!(envelope.unit, Unit::Paths);
    assert_eq!(envelope.reason, Some(Reason::Cap));
    assert_eq!(envelope.causes, vec![Reason::Cap]);
    assert_eq!(envelope.narrow, vec!["depth", "includeTests"]);

    // Mutation test: a mutation reporting depth or budget for this purely selective cut reds
    let envelope_mutated_depth = build_trace_to_envelope(15, 60, true, false).unwrap();
    assert_ne!(
        envelope_mutated_depth.reason,
        Some(Reason::Cap),
        "mutation adding depth must not report cap"
    );
    assert_eq!(
        envelope_mutated_depth.reason,
        Some(Reason::Depth),
        "mutation adding depth must report depth"
    );

    let envelope_mutated_budget = build_trace_to_envelope(15, 60, false, true).unwrap();
    assert_ne!(
        envelope_mutated_budget.reason,
        Some(Reason::Cap),
        "mutation adding budget must not report cap"
    );
    assert_eq!(
        envelope_mutated_budget.reason,
        Some(Reason::Budget),
        "mutation adding budget must report budget"
    );

    // Verify builder produces exact cap-only when neither depth nor budget fired
    let direct_envelope = build_trace_to_envelope(15, 60, false, false).unwrap();
    assert_eq!(direct_envelope.reason, Some(Reason::Cap));
    assert_eq!(direct_envelope.causes, vec![Reason::Cap]);
    assert_eq!(direct_envelope.total, Total::Exact(60));

    assert_no_bare_list_envelope(&data);
}

#[test]
fn test_cap_plus_depth_renders_depth_trailer_with_both_causes() {
    let data = load_fixture("cap_plus_depth.json");
    let resp = Response {
        id: "trace-5".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        callgraph_op: Some("trace_to".to_string()),
        ..Default::default()
    };

    let formatted = format_response_with_context("callgraph", &resp, &ctx);
    let expected_trailer = "shown 15 of ≥60 paths (depth) · narrow: depth, includeTests";
    assert!(
        formatted.contains(expected_trailer),
        "rendered text must contain depth trailer:\nexpected: {expected_trailer}\ngot: {formatted}"
    );

    let env_val = &data["paths_list_envelope"];
    let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(envelope.shown, 15);
    assert_eq!(envelope.total, Total::AtLeast(60));
    assert_eq!(envelope.unit, Unit::Paths);
    assert_eq!(envelope.reason, Some(Reason::Depth));
    assert_eq!(envelope.causes, vec![Reason::Depth, Reason::Cap]);
    assert_eq!(envelope.narrow, vec!["depth", "includeTests"]);

    assert_no_bare_list_envelope(&data);
}

#[test]
fn test_trace_data_capped_renders_hops_unit_and_asserts_unit_equality() {
    let data = load_fixture("trace_data_capped.json");
    let resp = Response {
        id: "trace-data-1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        callgraph_op: Some("trace_data".to_string()),
        ..Default::default()
    };

    let formatted = format_response_with_context("callgraph", &resp, &ctx);
    let expected_trailer = "shown 5 of ≥5 hops (depth) · narrow: depth";
    assert!(
        formatted.contains(expected_trailer),
        "rendered text must contain trace_data trailer:\nexpected: {expected_trailer}\ngot: {formatted}"
    );
    // Legacy "(depth limited)" clause should be suppressed when envelope is present
    assert!(
        !formatted.contains("(depth limited)"),
        "legacy (depth limited) clause must be suppressed when envelope is present"
    );

    let env_val = &data["hops_list_envelope"];
    let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(envelope.shown, 5);
    assert_eq!(envelope.total, Total::AtLeast(5));
    assert_eq!(envelope.unit, Unit::Hops);
    assert_eq!(envelope.reason, Some(Reason::Depth));
    assert_eq!(envelope.causes, vec![Reason::Depth]);
    assert_eq!(envelope.narrow, vec!["depth"]);

    // Assert text unit == envelope.unit == registered unit
    let text_unit = extract_trailer_unit(expected_trailer);
    let envelope_unit = envelope.unit.as_str();
    let surface_entry = find_surface("callgraph", "trace_data", TRACE_DATA_LIST_ID)
        .expect("registered surface entry for trace_data");
    let registered_unit = surface_entry.unit.as_str();

    assert_eq!(text_unit, "hops");
    assert_eq!(text_unit, envelope_unit);
    assert_eq!(envelope_unit, registered_unit);

    assert_no_bare_list_envelope(&data);
}

#[test]
fn test_complete_trace_replies_render_no_trailer_and_no_envelope_key() {
    // 1. Complete trace_to reply
    let complete_trace_to_data = load_fixture("complete_trace_to.json");
    let resp_trace_to = Response {
        id: "complete-trace-to".into(),
        success: true,
        data: complete_trace_to_data.clone(),
    };
    let ctx_trace_to = FormatContext {
        callgraph_op: Some("trace_to".to_string()),
        ..Default::default()
    };
    let formatted_trace_to =
        format_response_with_context("callgraph", &resp_trace_to, &ctx_trace_to);

    assert!(
        !formatted_trace_to.contains("shown "),
        "complete trace_to must not render any trailer: {formatted_trace_to}"
    );
    assert!(
        complete_trace_to_data.get("paths_list_envelope").is_none(),
        "complete trace_to must serialize no paths_list_envelope key"
    );

    // Builder returns None for complete trace_to
    assert_eq!(
        build_trace_to_envelope(1, 1, false, false),
        None,
        "complete trace_to must produce None envelope"
    );

    // 2. Complete trace_data reply
    let complete_trace_data = load_fixture("complete_trace_data.json");
    let resp_trace_data = Response {
        id: "complete-trace-data".into(),
        success: true,
        data: complete_trace_data.clone(),
    };
    let ctx_trace_data = FormatContext {
        callgraph_op: Some("trace_data".to_string()),
        ..Default::default()
    };
    let formatted_trace_data =
        format_response_with_context("callgraph", &resp_trace_data, &ctx_trace_data);

    assert!(
        !formatted_trace_data.contains("shown "),
        "complete trace_data must not render any trailer: {formatted_trace_data}"
    );
    assert!(
        complete_trace_data.get("hops_list_envelope").is_none(),
        "complete trace_data must serialize no hops_list_envelope key"
    );

    // Builder returns None for complete trace_data
    assert_eq!(
        build_trace_data_envelope(2, false),
        None,
        "complete trace_data must produce None envelope"
    );

    assert_no_bare_list_envelope(&complete_trace_to_data);
    assert_no_bare_list_envelope(&complete_trace_data);
}

// ---------------------------------------------------------------------------
// Transport Parity Tests
// ---------------------------------------------------------------------------

#[test]
fn test_transport_parity_across_ndjson_and_subc() {
    let capped_fixture_names = [
        "depth_exhaustion_no_path.json",
        "budget_exhaustion.json",
        "budget_plus_max_depth.json",
        "cap_only_r26.json",
        "cap_plus_depth.json",
    ];

    for name in capped_fixture_names {
        let data = load_fixture(name);
        let resp = Response {
            id: "parity-test".into(),
            success: true,
            data: data.clone(),
        };
        let ctx = FormatContext {
            callgraph_op: Some("trace_to".to_string()),
            ..Default::default()
        };

        let subc_rendered = format_response_with_context("callgraph", &resp, &ctx);
        let envelope: ListEnvelope =
            serde_json::from_value(data["paths_list_envelope"].clone()).unwrap();
        let expected_trailer = render_trailer(&envelope).unwrap();

        assert!(
            subc_rendered.contains(&expected_trailer),
            "subc formatted output for {name} must contain trailer {expected_trailer}"
        );

        // Pre-trailer base text for NDJSON parity
        let base_text = subc_rendered
            .strip_suffix(&format!("\n{expected_trailer}"))
            .unwrap_or(&subc_rendered);
        let ndjson_rendered = build_ndjson_text(base_text, &data, Some(TRACE_TO_LIST_ID), false);

        // Transport parity: both formats produce the identical trailer
        assert!(
            ndjson_rendered.contains(&expected_trailer),
            "ndjson rendered output for {name} must contain trailer {expected_trailer}"
        );
    }

    // Also test trace_data capped fixture parity
    let data_td = load_fixture("trace_data_capped.json");
    let resp_td = Response {
        id: "parity-trace-data".into(),
        success: true,
        data: data_td.clone(),
    };
    let ctx_td = FormatContext {
        callgraph_op: Some("trace_data".to_string()),
        ..Default::default()
    };
    let subc_td = format_response_with_context("callgraph", &resp_td, &ctx_td);
    let env_td: ListEnvelope =
        serde_json::from_value(data_td["hops_list_envelope"].clone()).unwrap();
    let expected_td_trailer = render_trailer(&env_td).unwrap();

    assert!(subc_td.contains(&expected_td_trailer));
    let base_td = subc_td
        .strip_suffix(&format!("\n{expected_td_trailer}"))
        .unwrap_or(&subc_td);
    let ndjson_td = build_ndjson_text(base_td, &data_td, Some(TRACE_DATA_LIST_ID), false);
    assert!(ndjson_td.contains(&expected_td_trailer));
}

// ---------------------------------------------------------------------------
// Wire Keys and Schema Tests
// ---------------------------------------------------------------------------

#[test]
fn test_wire_keys_derivation() {
    assert_eq!(derive_wire_key(TRACE_TO_LIST_ID, false), TRACE_TO_WIRE_KEY);
    assert_eq!(
        derive_wire_key(TRACE_DATA_LIST_ID, false),
        TRACE_DATA_WIRE_KEY
    );
}

#[test]
fn test_envelope_schema_for_all_capped_fixtures() {
    let capped_fixtures = [
        ("depth_exhaustion_no_path.json", TRACE_TO_WIRE_KEY),
        ("budget_exhaustion.json", TRACE_TO_WIRE_KEY),
        ("budget_plus_max_depth.json", TRACE_TO_WIRE_KEY),
        ("cap_only_r26.json", TRACE_TO_WIRE_KEY),
        ("cap_plus_depth.json", TRACE_TO_WIRE_KEY),
        ("trace_data_capped.json", TRACE_DATA_WIRE_KEY),
    ];

    for (name, key) in capped_fixtures {
        let data = load_fixture(name);
        let env_val = &data[key];
        assert!(
            env_val.is_object(),
            "fixture {name} must have envelope object at key {key}"
        );

        let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
        // reason == causes[0] always
        assert_eq!(
            envelope.reason,
            envelope.causes.first().copied(),
            "reason must equal causes[0] in {name}"
        );
        // causes ordered by precedence descending
        for i in 1..envelope.causes.len() {
            assert!(
                envelope.causes[i - 1].precedence() >= envelope.causes[i].precedence(),
                "causes must be in descending precedence order in {name}"
            );
        }
        // narrow is non-empty for trace surfaces
        assert!(
            !envelope.narrow.is_empty(),
            "trace narrow knobs must not be empty in {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Direct Adapter Attachment Tests
// ---------------------------------------------------------------------------

#[test]
fn test_adapter_attachment_functions() {
    let mut val = serde_json::json!({
        "paths": [],
        "total_paths": 0
    });
    let env = build_trace_to_envelope(0, 0, true, false);
    attach_trace_to_envelope(&mut val, env.as_ref());
    assert!(val.get(TRACE_TO_WIRE_KEY).is_some());

    let mut val_complete = serde_json::json!({
        "paths": [1],
        "total_paths": 1
    });
    let env_complete = build_trace_to_envelope(1, 1, false, false);
    attach_trace_to_envelope(&mut val_complete, env_complete.as_ref());
    assert!(val_complete.get(TRACE_TO_WIRE_KEY).is_none());

    let mut val_td = serde_json::json!({
        "hops": []
    });
    let env_td = build_trace_data_envelope(0, true);
    attach_trace_data_envelope(&mut val_td, env_td.as_ref());
    assert!(val_td.get(TRACE_DATA_WIRE_KEY).is_some());

    let mut val_td_complete = serde_json::json!({
        "hops": [1]
    });
    let env_td_complete = build_trace_data_envelope(1, false);
    attach_trace_data_envelope(&mut val_td_complete, env_td_complete.as_ref());
    assert!(val_td_complete.get(TRACE_DATA_WIRE_KEY).is_none());
}
