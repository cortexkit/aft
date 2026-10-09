use aft::list_envelope::{ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::find_surface;
use aft::list_surfaces::inspect::{
    attach_inspect_envelope, build_inspect_envelope, derive_inspect_wire_key,
    render_inspect_envelope_trailer, trailer_from_details, COMMAND,
};
use aft::list_surfaces::ReasonKind;
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::{Map, Value};

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/inspect/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture at {path}: {err}"));
    serde_json::from_str(&content)
        .unwrap_or_else(|err| panic!("failed to parse JSON from {path}: {err}"))
}

fn assert_no_bare_list_envelope(val: &Value) {
    match val {
        Value::Object(map) => {
            assert!(
                !map.contains_key("list_envelope"),
                "found forbidden bare key 'list_envelope'"
            );
            for v in map.values() {
                assert_no_bare_list_envelope(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                assert_no_bare_list_envelope(v);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 1. Surface Registry Tests
// ---------------------------------------------------------------------------

#[test]
fn surface_registry_inspect_entry() {
    let surface =
        find_surface(COMMAND, "", "payload.details").expect("inspect surface must be registered");
    assert_eq!(surface.command, "inspect");
    assert_eq!(surface.list_id, "payload.details");
    assert_eq!(surface.unit, Unit::Items);
    assert_eq!(surface.narrow, &["topK", "scope", "sections"]);

    // Must have Cap (Selecting)
    assert_eq!(surface.reasons.len(), 1);
    let cap_entry = &surface.reasons[0];
    assert_eq!(cap_entry.reason, Reason::Cap);
    assert_eq!(cap_entry.kind, ReasonKind::Selecting);
    assert!(cap_entry.predicate_name.contains("details_for"));
    assert!(cap_entry.predicate_name.contains("generated_details_for"));
    assert!(cap_entry.predicate_name.contains("test_only_details_for"));
    assert!(cap_entry.predicate_name.contains("topk_limiting"));
}

// ---------------------------------------------------------------------------
// 2. Acceptance Criteria: Four Capped Lists
// ---------------------------------------------------------------------------

#[test]
fn four_capped_lists_produces_four_sibling_envelopes_and_four_trailers() {
    let fixture = load_fixture("four_capped_lists");
    let data = &fixture["data"];
    assert_no_bare_list_envelope(&fixture);

    let details = data["details"].as_object().expect("details map");

    // Sibling envelopes with derivable keys
    let dead_code_env: ListEnvelope =
        serde_json::from_value(details["dead_code_list_envelope"].clone()).unwrap();
    let gen_env: ListEnvelope =
        serde_json::from_value(details["dead_code_generated_list_envelope"].clone()).unwrap();
    let test_only_env: ListEnvelope =
        serde_json::from_value(details["dead_code_test_only_list_envelope"].clone()).unwrap();
    let diag_env: ListEnvelope =
        serde_json::from_value(details["diagnostics_list_envelope"].clone()).unwrap();

    // Verify wire shape for each envelope
    assert_eq!(dead_code_env.shown, 2);
    assert_eq!(dead_code_env.total, Total::Exact(4));
    assert_eq!(dead_code_env.unit, Unit::Items);
    assert_eq!(dead_code_env.reason, Some(Reason::Cap));
    assert_eq!(dead_code_env.causes, vec![Reason::Cap]);
    assert_eq!(dead_code_env.narrow, vec!["topK", "scope", "sections"]);

    assert_eq!(gen_env.shown, 2);
    assert_eq!(gen_env.total, Total::Exact(3));
    assert_eq!(gen_env.unit, Unit::Items);
    assert_eq!(gen_env.reason, Some(Reason::Cap));
    assert_eq!(gen_env.causes, vec![Reason::Cap]);
    assert_eq!(gen_env.narrow, vec!["topK", "scope", "sections"]);

    assert_eq!(test_only_env.shown, 2);
    assert_eq!(test_only_env.total, Total::Exact(3));
    assert_eq!(test_only_env.unit, Unit::Items);
    assert_eq!(test_only_env.reason, Some(Reason::Cap));
    assert_eq!(test_only_env.causes, vec![Reason::Cap]);
    assert_eq!(test_only_env.narrow, vec!["topK", "scope", "sections"]);

    assert_eq!(diag_env.shown, 2);
    assert_eq!(diag_env.total, Total::Exact(4));
    assert_eq!(diag_env.unit, Unit::Items);
    assert_eq!(diag_env.reason, Some(Reason::Cap));
    assert_eq!(diag_env.causes, vec![Reason::Cap]);
    assert_eq!(diag_env.narrow, vec!["topK", "scope", "sections"]);

    // Derive wire keys
    assert_eq!(
        derive_inspect_wire_key("dead_code"),
        "dead_code_list_envelope"
    );
    assert_eq!(
        derive_inspect_wire_key("dead_code_generated"),
        "dead_code_generated_list_envelope"
    );
    assert_eq!(
        derive_inspect_wire_key("dead_code_test_only"),
        "dead_code_test_only_list_envelope"
    );
    assert_eq!(
        derive_inspect_wire_key("diagnostics"),
        "diagnostics_list_envelope"
    );

    // Exactly four sibling envelopes in details (count of keys ending in _list_envelope)
    let envelope_count = details
        .keys()
        .filter(|k| k.ends_with("_list_envelope"))
        .count();
    assert_eq!(envelope_count, 4);

    // Format response through subc formatter
    let resp = Response {
        id: "1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("inspect", &resp, &ctx);

    // Trailer count: exactly 4 trailer lines in text
    let trailer_lines: Vec<&str> = formatted
        .lines()
        .filter(|line| line.starts_with("shown ") && line.contains(" items (cap)"))
        .collect();
    assert_eq!(
        trailer_lines.len(),
        4,
        "must have exactly four trailer lines, found {}:\n{formatted}",
        trailer_lines.len()
    );

    // R10 placement checks:
    // 1. Trailer 1 right after dead_code last row and before generated heading
    let trailer1 = "shown 2 of 4 items (cap) · narrow: topK, scope, sections";
    assert!(formatted.contains(&format!("  src/b.rs::beta\n{trailer1}\n  generated: 3:")));

    // 2. Trailer 2 right after generated last row and before test-only usage heading
    let trailer2 = "shown 2 of 3 items (cap) · narrow: topK, scope, sections";
    assert!(formatted.contains(&format!(
        "    src/gen_b.rs::gen_two\n{trailer2}\n  test-only usage: 3:"
    )));

    // 3. Trailer 3 right after test-only last row
    assert!(formatted.contains(&format!(
        "    src/test_b.rs::t_two — used by tests/test_b.rs\n{trailer2}"
    )));

    // 4. Auto-emitted diagnostics render last, with trailer 4 after its last row
    assert!(formatted.contains(&format!(
        "- src/diag_b.rs:20:8 error missing type [rustc]\n{trailer1}"
    )));
    assert!(
        formatted.ends_with(trailer1),
        "diagnostics trailer must be last line of text: {formatted}"
    );
}

// ---------------------------------------------------------------------------
// 3. Acceptance Criteria: Empty Capped List (R10)
// ---------------------------------------------------------------------------

#[test]
fn empty_capped_list_renders_heading_then_trailer() {
    let fixture = load_fixture("empty_capped_list");
    let data = &fixture["data"];
    assert_no_bare_list_envelope(&fixture);

    let details = data["details"].as_object().expect("details map");
    let envelope_count = details
        .keys()
        .filter(|k| k.ends_with("_list_envelope"))
        .count();
    assert_eq!(envelope_count, 4);

    let resp = Response {
        id: "2".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("inspect", &resp, &ctx);

    // Each empty capped list renders heading then trailer:
    // 1. Dead code heading then trailer
    assert!(formatted.contains(
        "Dead code: 3 (generated: 2):\nshown 0 of 3 items (cap) · narrow: topK, scope, sections"
    ));

    // 2. Generated heading then trailer
    assert!(formatted
        .contains("  generated: 2:\nshown 0 of 2 items (cap) · narrow: topK, scope, sections"));

    // 3. Test-only heading then trailer
    assert!(formatted.contains(
        "  test-only usage: 2:\nshown 0 of 2 items (cap) · narrow: topK, scope, sections"
    ));

    // 4. Diagnostics details heading then trailer
    assert!(formatted.contains(
        "diagnostics details:\nshown 0 of 2 items (cap) · narrow: topK, scope, sections"
    ));
}

// ---------------------------------------------------------------------------
// 4. Acceptance Criteria: Uncapped Replies
// ---------------------------------------------------------------------------

#[test]
fn uncapped_inspect_replies_render_no_trailer_and_serialize_no_envelope() {
    let fixture = load_fixture("complete_uncapped");
    let data = &fixture["data"];
    assert_no_bare_list_envelope(&fixture);

    let details = data["details"].as_object().expect("details map");

    // No envelope keys in details
    let envelope_keys: Vec<&String> = details
        .keys()
        .filter(|k| k.ends_with("_list_envelope"))
        .collect();
    assert!(
        envelope_keys.is_empty(),
        "uncapped reply must serialize no envelope key: {envelope_keys:?}"
    );

    let resp = Response {
        id: "3".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("inspect", &resp, &ctx);

    // No trailer rendered
    assert!(
        !formatted.contains("(cap)"),
        "uncapped reply must render no trailer: {formatted}"
    );
    assert!(
        !formatted.contains("shown "),
        "uncapped reply must render no trailer: {formatted}"
    );

    // Base text in fixture equals formatted subc text (pre-spec identity)
    let base_text = data["text"].as_str().unwrap();
    assert_eq!(formatted.trim(), base_text.trim());
}

// ---------------------------------------------------------------------------
// 5. Todos Capped
// ---------------------------------------------------------------------------

#[test]
fn todos_capped_produces_envelope_and_trailer() {
    let fixture = load_fixture("todos_capped");
    let data = &fixture["data"];
    assert_no_bare_list_envelope(&fixture);

    let details = data["details"].as_object().expect("details map");
    let env: ListEnvelope = serde_json::from_value(details["todos_list_envelope"].clone()).unwrap();
    assert_eq!(env.shown, 2);
    assert_eq!(env.total, Total::Exact(3));
    assert_eq!(env.unit, Unit::Items);
    assert_eq!(env.reason, Some(Reason::Cap));

    let resp = Response {
        id: "4".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("inspect", &resp, &ctx);

    assert!(formatted.contains("shown 2 of 3 items (cap) · narrow: topK, scope, sections"));
    assert!(formatted.contains(
        "  src/b.ts:20 TODO clean up\nshown 2 of 3 items (cap) · narrow: topK, scope, sections"
    ));
}

// ---------------------------------------------------------------------------
// 6. Duplicates Capped
// ---------------------------------------------------------------------------

#[test]
fn duplicates_capped_produces_main_and_generated_envelopes() {
    let fixture = load_fixture("duplicates_capped");
    let data = &fixture["data"];
    assert_no_bare_list_envelope(&fixture);

    let details = data["details"].as_object().expect("details map");
    let main_env: ListEnvelope =
        serde_json::from_value(details["duplicates_list_envelope"].clone()).unwrap();
    let gen_env: ListEnvelope =
        serde_json::from_value(details["duplicates_generated_list_envelope"].clone()).unwrap();

    assert_eq!(main_env.shown, 2);
    assert_eq!(main_env.total, Total::Exact(3));
    assert_eq!(gen_env.shown, 1);
    assert_eq!(gen_env.total, Total::Exact(2));

    let resp = Response {
        id: "5".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("inspect", &resp, &ctx);

    assert!(formatted.contains("shown 2 of 3 items (cap) · narrow: topK, scope, sections"));
    assert!(formatted.contains("shown 1 of 2 items (cap) · narrow: topK, scope, sections"));
}

// ---------------------------------------------------------------------------
// 7. Transport Parity Across subc and NDJSON
// ---------------------------------------------------------------------------

#[test]
fn transport_parity_for_inspect_fixtures() {
    for fixture_name in [
        "four_capped_lists",
        "empty_capped_list",
        "complete_uncapped",
        "todos_capped",
        "duplicates_capped",
    ] {
        let fixture = load_fixture(fixture_name);
        let data = &fixture["data"];
        let resp = Response {
            id: format!("transport-{fixture_name}"),
            success: true,
            data: data.clone(),
        };
        let ctx = FormatContext::default();
        let subc_text = format_response_with_context("inspect", &resp, &ctx);

        let base_text = data.get("text").and_then(Value::as_str).unwrap_or("");
        let ndjson_text = build_ndjson_text(base_text, data, None, false);

        assert_eq!(
            subc_text.trim(),
            ndjson_text.trim(),
            "transport parity mismatch for fixture '{fixture_name}'"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. Summary Count Arrays Carry No Envelope
// ---------------------------------------------------------------------------

#[test]
fn summary_count_arrays_carry_no_envelope() {
    for fixture_name in [
        "four_capped_lists",
        "empty_capped_list",
        "complete_uncapped",
    ] {
        let fixture = load_fixture(fixture_name);
        let data = &fixture["data"];
        if let Some(summary) = data.get("summary").and_then(Value::as_object) {
            for (category, cat_val) in summary {
                if let Some(obj) = cat_val.as_object() {
                    for key in obj.keys() {
                        assert!(
                            !key.ends_with("_list_envelope"),
                            "summary for '{category}' must not carry envelope key '{key}' in fixture '{fixture_name}'"
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Adapter Functions Unit Tests
// ---------------------------------------------------------------------------

#[test]
fn build_inspect_envelope_logic() {
    // Uncapped: shown == total -> None
    assert!(build_inspect_envelope(5, 5).is_none());
    assert!(build_inspect_envelope(10, 5).is_none());

    // Capped: shown < total -> Some
    let env = build_inspect_envelope(3, 7).expect("capped envelope");
    assert_eq!(env.shown, 3);
    assert_eq!(env.total, Total::Exact(7));
    assert_eq!(env.unit, Unit::Items);
    assert_eq!(env.reason, Some(Reason::Cap));
    assert_eq!(env.causes, vec![Reason::Cap]);
    assert_eq!(env.narrow, vec!["topK", "scope", "sections"]);

    let rendered = render_inspect_envelope_trailer(&env, "dead_code").unwrap();
    assert_eq!(
        rendered,
        "shown 3 of 7 items (cap) · narrow: topK, scope, sections"
    );
}

#[test]
fn attach_inspect_envelope_mutates_details_when_capped() {
    let mut details = Map::new();
    let attached = attach_inspect_envelope(&mut details, "dead_code", 2, 5);
    assert!(attached.is_some());
    assert!(details.contains_key("dead_code_list_envelope"));

    // Final pages retain structured pagination metadata, never a text trailer.
    let mut details_uncapped = Map::new();
    let attached = attach_inspect_envelope(&mut details_uncapped, "dead_code", 5, 5);
    assert!(attached.is_some());
    assert_eq!(
        details_uncapped["dead_code_list_envelope"]["next_offset"],
        Value::Null
    );
    assert_eq!(trailer_from_details(&details_uncapped, "dead_code"), None);
}

#[test]
fn trailer_from_details_helper() {
    let mut details = Map::new();
    attach_inspect_envelope(&mut details, "dead_code", 1, 4);

    let trailer = trailer_from_details(&details, "dead_code");
    assert_eq!(
        trailer,
        Some("shown 1 of 4 items (cap) · narrow: topK, scope, sections".to_string())
    );

    let uncapped_trailer = trailer_from_details(&details, "unknown_category");
    assert_eq!(uncapped_trailer, None);
}

#[test]
fn all_registered_inspect_category_lists_derivable_keys() {
    let lists = [
        "dead_code",
        "dead_code_test_only",
        "dead_code_generated",
        "unused_exports",
        "unused_exports_test_only",
        "unused_exports_generated",
        "duplicates",
        "duplicates_generated",
        "complexity",
        "cycles",
        "todos",
        "diagnostics",
    ];

    for list_key in lists {
        let wire_key = derive_inspect_wire_key(list_key);
        assert_eq!(
            wire_key,
            format!("{list_key}_list_envelope"),
            "wire key must be derivable as <list_key>_list_envelope"
        );
        let env = build_inspect_envelope(1, 2).unwrap();
        let trailer = render_inspect_envelope_trailer(&env, list_key).unwrap();
        assert_eq!(
            trailer, "shown 1 of 2 items (cap) · narrow: topK, scope, sections",
            "trailer for {list_key} must follow unified grammar"
        );
    }
}
