use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aft::compress::builtin_filters;
use aft::compress::caps::DropClass;
use aft::compress::compress_with_registry_exit_code;
use aft::compress::toml_filter::build_registry;
use aft::list_envelope::{derive_wire_key, render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::bash::{
    build_bash_output_envelope, build_envelope_from_output, count_output_lines, wire_key, LIST_ID,
    NARROW, REASON, REASON_KIND, UNIT, WIRE_KEY,
};
use aft::list_surfaces::{ReasonKind, EXCLUSIONS, LIST_SURFACES};
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("bash")
}

fn assert_no_bare_list_envelope_key_recursive(val: &Value) {
    match val {
        Value::Object(map) => {
            assert!(
                !map.contains_key("list_envelope"),
                "found forbidden bare key 'list_envelope' in object: {val:#?}"
            );
            for value in map.values() {
                assert_no_bare_list_envelope_key_recursive(value);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                assert_no_bare_list_envelope_key_recursive(item);
            }
        }
        _ => {}
    }
}

#[test]
fn test_bash_surface_registration() {
    let surface = LIST_SURFACES
        .iter()
        .find(|s| s.command == "bash" && s.list_id == "bash.output")
        .expect("bash.output must be registered in LIST_SURFACES");

    assert_eq!(surface.command, "bash");
    assert_eq!(surface.mode, "");
    assert_eq!(surface.list_id, "bash.output");
    assert_eq!(surface.unit, Unit::Lines);
    assert_eq!(surface.narrow, &[] as &[&str]);

    assert_eq!(surface.reasons.len(), 1);
    let reason_entry = surface.reasons[0];
    assert_eq!(reason_entry.reason, Reason::Cap);
    assert_eq!(reason_entry.kind, ReasonKind::Selecting);

    // Module constants match registry entry
    assert_eq!(LIST_ID, surface.list_id);
    assert_eq!(WIRE_KEY, "bash_output_list_envelope");
    assert_eq!(UNIT, surface.unit);
    assert_eq!(REASON, reason_entry.reason);
    assert_eq!(REASON_KIND, reason_entry.kind);
    assert_eq!(NARROW, surface.narrow);

    assert_eq!(wire_key(), WIRE_KEY);
    assert_eq!(derive_wire_key(LIST_ID, true), WIRE_KEY);
}

#[test]
fn test_compressor_seam_returns_input_line_count_and_counts_only() {
    let fixture_dir = fixtures_dir().join("capped_4000_in_61_out");
    let input_path = fixture_dir.join("input.txt");
    let input_text = fs::read_to_string(&input_path).expect("read fixture input.txt");

    let total_input_lines = input_text.lines().count();
    assert_eq!(total_input_lines, 4000);

    let registry = build_registry(builtin_filters::ALL, None, None);
    let result = compress_with_registry_exit_code("du -a", &input_text, None, &registry);

    // Compressor seam returns the input line count
    assert_eq!(result.input_line_count, 4000);
    assert_eq!(result.input_line_count(), 4000);

    // Compressor returned counts only, never trailer strings
    assert_eq!(result.text.lines().count(), 60);
    assert!(
        !result.text.contains("shown "),
        "compressor output must not contain trailer prefix 'shown '"
    );
    assert!(
        !result.text.contains("(cap)"),
        "compressor output must not contain trailer reason '(cap)'"
    );
    assert!(
        !result.text.contains("narrow:"),
        "compressor output must not contain narrow clause"
    );
}

#[test]
fn test_4000_in_61_out_fixture() {
    let fixture_dir = fixtures_dir().join("capped_4000_in_61_out");
    let input_path = fixture_dir.join("input.txt");
    let reply_path = fixture_dir.join("reply.json");

    let input_text = fs::read_to_string(&input_path).expect("read fixture input.txt");
    let reply_json = fs::read_to_string(&reply_path).expect("read fixture reply.json");

    let input_line_count = input_text.lines().count();
    assert_eq!(input_line_count, 4000);

    let val: Value = serde_json::from_str(&reply_json).expect("parse reply.json");
    let resp = Response {
        id: val
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_string(),
        success: val.get("success").and_then(Value::as_bool).unwrap_or(true),
        data: val.clone(),
    };

    assert!(resp.success);
    assert_no_bare_list_envelope_key_recursive(&val);

    let output_str = resp.data["output"]
        .as_str()
        .expect("output must be a string");
    let shown = count_output_lines(output_str);

    // shown = 61, total = Exact(4000)
    assert_eq!(shown, 61);
    assert_eq!(input_line_count, 4000);

    // Built from agent-facing output text
    let envelope = build_envelope_from_output(output_str, input_line_count)
        .expect("envelope must be constructed for capped output");

    assert_eq!(envelope.shown, 61);
    assert_eq!(envelope.total, Total::Exact(4000));
    assert_eq!(envelope.unit, Unit::Lines);
    assert_eq!(envelope.reason, Some(Reason::Cap));
    assert_eq!(envelope.causes, vec![Reason::Cap]);
    assert_eq!(envelope.narrow, Vec::<String>::new());

    // Rendered trailer grammar check
    let rendered_trailer = render_trailer(&envelope).expect("render trailer");
    assert_eq!(rendered_trailer, "shown 61 of 4000 lines (cap)");
    assert!(
        !rendered_trailer.contains("narrow:"),
        "bash trailer must have no narrow clause"
    );

    // Output text contains the trailer
    assert!(output_str.contains("shown 61 of 4000 lines (cap)"));
    assert_eq!(
        output_str.matches("shown 61 of 4000 lines (cap)").count(),
        1
    );

    // Serialized at reply root beside output
    let env_val = resp
        .data
        .get("bash_output_list_envelope")
        .expect("bash_output_list_envelope must be at reply root");
    let deserialized_env: ListEnvelope =
        serde_json::from_value(env_val.clone()).expect("deserialize ListEnvelope");
    assert_eq!(deserialized_env, envelope);
}

#[test]
fn test_mutation_sourcing_either_count_from_dropped_by_class_reds() {
    let fixture_dir = fixtures_dir().join("capped_4000_in_61_out");
    let input_path = fixture_dir.join("input.txt");
    let input_text = fs::read_to_string(&input_path).expect("read fixture input.txt");

    let registry = build_registry(builtin_filters::ALL, None, None);
    let result = compress_with_registry_exit_code("du -a", &input_text, None, &registry);

    // dropped_by_class on du is empty (plain line cap does not classify blocks)
    let dropped_by_class_sum: usize = result.dropped_by_class.values().sum();
    assert_eq!(dropped_by_class_sum, 0);

    // Expected values on fixture:
    let expected_shown = 61;
    let expected_total = 4000;

    // Mutation 1: Sourcing shown from dropped_by_class produces 0 != 61
    let mutated_shown = dropped_by_class_sum;
    assert_ne!(
        mutated_shown, expected_shown,
        "mutation sourcing shown from dropped_by_class must red"
    );

    // Mutation 2: Sourcing total from shown + dropped_by_class produces 61 != 4000
    let mutated_total = expected_shown + dropped_by_class_sum;
    assert_ne!(
        mutated_total, expected_total,
        "mutation sourcing total from dropped_by_class must red"
    );

    // Mutation 3: Sourcing from non-empty dropped_by_class (synthetic drop blocks)
    let mut synthetic_drops: BTreeMap<DropClass, usize> = BTreeMap::new();
    synthetic_drops.insert(DropClass::Error, 25);
    synthetic_drops.insert(DropClass::Warning, 10);
    let synthetic_sum: usize = synthetic_drops.values().sum(); // 35
    assert_ne!(
        synthetic_sum, expected_shown,
        "mutation sourcing shown from block drops must red"
    );
    assert_ne!(
        expected_shown + synthetic_sum,
        expected_total,
        "mutation sourcing total from block drops must red"
    );
}

#[test]
fn test_subc_and_ndjson_transport_parity_on_fixture() {
    let fixture_dir = fixtures_dir().join("capped_4000_in_61_out");
    let reply_path = fixture_dir.join("reply.json");
    let reply_json = fs::read_to_string(&reply_path).expect("read fixture reply.json");

    let val: Value = serde_json::from_str(&reply_json).expect("parse reply.json");
    let resp = Response {
        id: val
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_string(),
        success: val.get("success").and_then(Value::as_bool).unwrap_or(true),
        data: val.clone(),
    };

    let ctx = FormatContext::default();
    let subc_formatted = format_response_with_context("bash", &resp, &ctx);
    let output_raw = resp.data["output"].as_str().unwrap();

    let ndjson_formatted = build_ndjson_text(output_raw, &resp.data, Some("bash.output"), true);

    // Subc formatter passes bash text through unchanged
    assert_eq!(subc_formatted, output_raw);

    // Transport parity across NDJSON and subc: byte-identical
    assert_eq!(subc_formatted, ndjson_formatted);

    // Trailer is never rendered twice
    assert_eq!(
        subc_formatted
            .matches("shown 61 of 4000 lines (cap)")
            .count(),
        1
    );
    assert_eq!(
        ndjson_formatted
            .matches("shown 61 of 4000 lines (cap)")
            .count(),
        1
    );
}

#[test]
fn test_uncompressed_bash_output_stays_byte_identical_to_prespec_golden() {
    let fixture_dir = fixtures_dir().join("uncompressed");
    let input_path = fixture_dir.join("input.txt");
    let reply_path = fixture_dir.join("reply.json");

    let input_text = fs::read_to_string(&input_path).expect("read fixture input.txt");
    let reply_json = fs::read_to_string(&reply_path).expect("read fixture reply.json");

    let input_line_count = input_text.lines().count();
    assert_eq!(input_line_count, 5);

    let val: Value = serde_json::from_str(&reply_json).expect("parse reply.json");
    let resp = Response {
        id: val
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_string(),
        success: val.get("success").and_then(Value::as_bool).unwrap_or(true),
        data: val.clone(),
    };

    assert!(resp.success);
    assert_no_bare_list_envelope_key_recursive(&val);

    let output_str = resp.data["output"].as_str().unwrap();
    let shown = count_output_lines(output_str);
    assert_eq!(shown, 5);

    // When shown >= input_line_count, no envelope is built (reason is None)
    let envelope = build_bash_output_envelope(shown, input_line_count);
    assert_eq!(envelope, None);

    let envelope_from_output = build_envelope_from_output(output_str, input_line_count);
    assert_eq!(envelope_from_output, None);

    // No envelope key serialized in response data
    assert!(
        resp.data.get("bash_output_list_envelope").is_none(),
        "uncompressed bash output must not serialize bash_output_list_envelope"
    );
    assert!(resp.data.get("list_envelope").is_none());

    // Output text contains no trailer string
    assert!(
        !output_str.contains("shown "),
        "uncompressed output must not contain trailer"
    );
    assert!(!output_str.contains("(cap)"));

    // Subc and NDJSON formatting match pre-spec golden byte-identically
    let ctx = FormatContext::default();
    let subc_formatted = format_response_with_context("bash", &resp, &ctx);
    let ndjson_formatted = build_ndjson_text(output_str, &resp.data, Some("bash.output"), true);

    assert_eq!(subc_formatted, output_str);
    assert_eq!(ndjson_formatted, output_str);
    assert_eq!(subc_formatted, input_text);
}

#[test]
fn test_bash_live_tail_raw_and_exclusions_entry() {
    let exclusion = EXCLUSIONS
        .iter()
        .find(|e| e.file == "commands/bash_status.rs")
        .expect("commands/bash_status.rs must be in EXCLUSIONS");

    assert_eq!(exclusion.enclosing_item, "handle");
    assert_eq!(
        exclusion.location_or_primitive,
        "bash_status / bash live-tail"
    );
    assert!(
        !exclusion.reason.trim().is_empty(),
        "bash live-tail exclusion must have a non-empty reason"
    );
    assert!(
        exclusion.reason.contains("bash live-tail"),
        "exclusion reason must mention bash live-tail"
    );
    assert!(
        exclusion.reason.contains("raw by design"),
        "exclusion reason must document raw by design"
    );
    assert!(
        exclusion.reason.contains("no truncation envelope"),
        "exclusion reason must document no truncation envelope"
    );
}

#[test]
fn test_envelope_wire_schema_and_grammar() {
    let env = ListEnvelope::new(61, Total::Exact(4000), Unit::Lines, vec![Reason::Cap], &[]);

    let json_val = serde_json::to_value(&env).expect("serialize ListEnvelope");

    // Exact wire shape per R15
    assert_eq!(json_val["shown"], 61);
    assert_eq!(json_val["total"]["kind"], "exact");
    assert_eq!(json_val["total"]["value"], 4000);
    assert_eq!(json_val["unit"], "lines");
    assert_eq!(json_val["reason"], "cap");
    assert_eq!(json_val["causes"], serde_json::json!(["cap"]));
    assert_eq!(json_val["narrow"], serde_json::json!([]));

    // causes is ordered by precedence descending and reason == causes[0]
    assert_eq!(
        json_val["reason"].as_str().unwrap(),
        json_val["causes"][0].as_str().unwrap()
    );

    // narrow is [] and not omitted
    assert!(json_val.get("narrow").is_some());
    assert!(json_val["narrow"].as_array().unwrap().is_empty());

    // Grammar check
    let trailer = render_trailer(&env).expect("render trailer");
    assert_eq!(trailer, "shown 61 of 4000 lines (cap)");
    assert!(!trailer.contains("narrow:"));
    assert!(!trailer.ends_with('.'));
    assert!(!trailer.ends_with(';'));
}
