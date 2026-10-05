//! Integration tests for grep list surface truncation contract.

use std::fs;
use std::path::{Path, PathBuf};

use aft::list_envelope::{derive_wire_key, render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::grep::{
    build_grep_envelope, build_grep_envelope_from_parts, COMMAND, LIST_ID, NARROW, UNIT,
};
use aft::list_surfaces::{find_surface, ReasonKind};
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, rendered_grep_match_count, FormatContext};
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("grep")
}

fn load_fixture(filename: &str) -> Value {
    let path = fixtures_dir().join(filename);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture at {}: {err}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|err| panic!("failed to parse JSON from {}: {err}", path.display()))
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
fn grep_scan_deadline_reports_a_lower_bound_total() {
    // An indexed scan that ran out of time has unread candidate files, so
    // even an uncapped page must not claim an exact total.
    let payload = serde_json::json!({
        "matches": [
            {"file": "a.rs", "line": 1, "column": 1, "line_text": "x", "match_text": "x"},
        ],
        "total_matches": 1,
        "truncated": false,
        "scan_deadline_reached": true,
        "skipped_foreign_mounts": 0,
    });
    let envelope = build_grep_envelope(&payload).expect("a timed-out scan is not complete");
    assert_eq!(envelope.reason, Some(Reason::Walk));
    assert_eq!(envelope.total, Total::AtLeast(1));

    let complete = serde_json::json!({
        "matches": payload["matches"].clone(),
        "total_matches": 1,
        "truncated": false,
        "skipped_foreign_mounts": 0,
    });
    assert!(build_grep_envelope(&complete).is_none());
}

#[test]
fn grep_surface_metadata_matches_registry() {
    assert_eq!(COMMAND, "grep");
    assert_eq!(LIST_ID, "payload.matches");
    assert_eq!(UNIT, Unit::Rows);
    assert_eq!(NARROW, &["offset", "path", "include", "exclude"]);

    let surface = find_surface("grep", "", "payload.matches").expect("grep surface registered");
    assert_eq!(surface.unit, Unit::Rows);
    assert_eq!(surface.narrow, &["offset", "path", "include", "exclude"]);

    let walk_entry = surface
        .reasons
        .iter()
        .find(|r| r.reason == Reason::Walk)
        .expect("walk reason registered");
    assert_eq!(walk_entry.kind, ReasonKind::Bounding);

    let cap_entry = surface
        .reasons
        .iter()
        .find(|r| r.reason == Reason::Cap)
        .expect("cap reason registered");
    assert_eq!(cap_entry.kind, ReasonKind::Selecting);
}

#[test]
fn wire_key_is_matches_list_envelope() {
    assert_eq!(derive_wire_key(LIST_ID, false), "matches_list_envelope");
}

#[test]
fn cap_on_finished_walk_renders_at_least_floor() {
    let fixture = load_fixture("cap_100_of_1204.json");
    let envelope = build_grep_envelope(&fixture).expect("envelope present");

    // Finished walk hitting executor cap: total is a floor (AtLeast)
    assert_eq!(envelope.shown, 100);
    assert_eq!(envelope.total, Total::AtLeast(1204));
    assert_eq!(envelope.unit, Unit::Rows);
    assert_eq!(envelope.reason, Some(Reason::Cap));
    assert_eq!(envelope.causes, vec![Reason::Cap]);
    assert_eq!(
        envelope.narrow,
        vec![
            "offset".to_string(),
            "path".to_string(),
            "include".to_string(),
            "exclude".to_string()
        ]
    );

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 100 of ≥1204 rows (cap) · narrow: offset, path, include, exclude"
    );

    // Formatter suppresses (capped) and appends the trailer
    let resp = Response {
        id: "cap-1".into(),
        success: true,
        data: fixture.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("grep", &resp, &ctx);
    assert!(!formatted.contains("(capped)"));
    assert!(formatted
        .contains("shown 100 of ≥1204 rows (cap) · narrow: offset, path, include, exclude"));

    // NDJSON text builder produces the same trailer
    let base_text = fixture["text"].as_str().unwrap();
    let ndjson = build_ndjson_text(base_text, &fixture, Some("payload.matches"), false);
    assert!(
        ndjson.contains("shown 100 of ≥1204 rows (cap) · narrow: offset, path, include, exclude")
    );
}

#[test]
fn walk_truncated_and_cap_renders_mixed_cause() {
    let fixture = load_fixture("walk_truncated_and_cap.json");

    // Both flags must be present in JSON
    assert_eq!(fixture["walk_truncated"], true);
    assert_eq!(fixture["truncated"], true);

    let envelope = build_grep_envelope(&fixture).expect("envelope present");
    assert_eq!(envelope.shown, 100);
    assert_eq!(envelope.total, Total::AtLeast(100));
    assert_eq!(envelope.unit, Unit::Rows);
    assert_eq!(envelope.reason, Some(Reason::Walk));
    assert_eq!(envelope.causes, vec![Reason::Walk, Reason::Cap]);
    assert_eq!(
        envelope.narrow,
        vec![
            "offset".to_string(),
            "path".to_string(),
            "include".to_string(),
            "exclude".to_string()
        ]
    );

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 100 of ≥100 rows (walk) · narrow: offset, path, include, exclude"
    );

    let resp = Response {
        id: "walk-cap-1".into(),
        success: true,
        data: fixture,
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("grep", &resp, &ctx);
    assert!(formatted
        .contains("shown 100 of ≥100 rows (walk) · narrow: offset, path, include, exclude"));
}

#[test]
fn skipped_foreign_mounts_renders_walk_and_is_independent_of_walk_truncated() {
    let fixture = load_fixture("skipped_foreign_mounts_walk.json");

    // skipped_foreign_mounts set without walk_truncated or executor cap
    assert!(fixture["skipped_foreign_mounts"].as_u64().unwrap_or(0) > 0);
    assert_ne!(fixture.get("walk_truncated"), Some(&Value::Bool(true)));
    assert_ne!(fixture.get("truncated"), Some(&Value::Bool(true)));

    let envelope = build_grep_envelope(&fixture).expect("envelope present");
    assert_eq!(envelope.shown, 12);
    assert_eq!(envelope.total, Total::AtLeast(12));
    assert_eq!(envelope.unit, Unit::Rows);
    assert_eq!(envelope.reason, Some(Reason::Walk));
    assert_eq!(envelope.causes, vec![Reason::Walk]);

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 12 of ≥12 rows (walk) · narrow: offset, path, include, exclude"
    );

    let resp = Response {
        id: "mounts-1".into(),
        success: true,
        data: fixture,
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("grep", &resp, &ctx);
    assert!(
        formatted.contains("shown 12 of ≥12 rows (walk) · narrow: offset, path, include, exclude")
    );

    // Verify independence: neither walk_truncated alone nor skipped_foreign_mounts alone renders as complete.
    let env_walk_only = build_grep_envelope_from_parts(10, 10, 10, false, true, 0)
        .expect("walk_truncated alone must produce an envelope");
    assert_eq!(env_walk_only.reason, Some(Reason::Walk));
    assert!(env_walk_only.total.is_at_least());

    let env_mounts_only = build_grep_envelope_from_parts(10, 10, 10, false, false, 1)
        .expect("skipped_foreign_mounts alone must produce an envelope");
    assert_eq!(env_mounts_only.reason, Some(Reason::Walk));
    assert!(env_mounts_only.total.is_at_least());

    let env_neither = build_grep_envelope_from_parts(10, 10, 10, false, false, 0);
    assert!(
        env_neither.is_none(),
        "neither flag set and uncapped must be complete"
    );
}

#[test]
fn display_selector_thinned_renders_exact_total() {
    let fixture = load_fixture("display_thinned_25_of_42.json");

    let match_array = fixture["matches"].as_array().expect("matches array");
    assert_eq!(match_array.len(), 42);
    assert_eq!(fixture["total_matches"], 42);
    assert_ne!(fixture.get("truncated"), Some(&Value::Bool(true)));

    // Seam reports the 25 rows that fit the output byte budget out of the
    // 42-row page (`rendered_matches`), not the page length.
    let rendered_rows = rendered_grep_match_count(&fixture);
    assert_eq!(rendered_rows, 25);

    let envelope = build_grep_envelope(&fixture).expect("envelope present");
    assert_eq!(envelope.shown, 25);
    assert_eq!(envelope.total, Total::Exact(42));
    assert_eq!(envelope.unit, Unit::Rows);
    assert_eq!(envelope.reason, Some(Reason::Cap));
    assert_eq!(envelope.causes, vec![Reason::Cap]);

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 25 of 42 rows (cap) · narrow: offset, path, include, exclude"
    );

    // Formatter renders the trailer
    let resp = Response {
        id: "display-1".into(),
        success: true,
        data: fixture.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("grep", &resp, &ctx);
    assert!(
        formatted.contains("shown 25 of 42 rows (cap) · narrow: offset, path, include, exclude")
    );

    // JSON match array remains completely unchanged
    assert_eq!(fixture["matches"].as_array().unwrap().len(), 42);
}

#[test]
fn mutation_computing_shown_from_match_array_length_reds() {
    let fixture = load_fixture("display_thinned_25_of_42.json");
    let match_array_len = fixture["matches"].as_array().unwrap().len();
    assert_eq!(match_array_len, 42);

    let real_envelope = build_grep_envelope(&fixture).expect("envelope present");
    assert_eq!(real_envelope.shown, 25);

    // If shown were computed from the match array length instead of the seam count:
    let mutated_shown = match_array_len;
    let mutated_envelope = ListEnvelope::new(
        mutated_shown,
        Total::Exact(42),
        Unit::Rows,
        vec![Reason::Cap],
        &["offset", "path", "include", "exclude"],
    );

    let real_trailer = render_trailer(&real_envelope).unwrap();
    let mutated_trailer = render_trailer(&mutated_envelope).unwrap();

    assert_eq!(
        real_trailer,
        "shown 25 of 42 rows (cap) · narrow: offset, path, include, exclude"
    );
    assert_eq!(
        mutated_trailer,
        "shown 42 of 42 rows (cap) · narrow: offset, path, include, exclude"
    );

    // Assert that the mutation would fail the required assertion
    assert_ne!(
        mutated_trailer, real_trailer,
        "computing shown from array length must diverge from correct seam count"
    );
}

#[test]
fn complete_reply_byte_identical_to_golden() {
    let fixture = load_fixture("complete_untruncated.json");

    // Complete reply produces no envelope
    let envelope = build_grep_envelope(&fixture);
    assert!(
        envelope.is_none(),
        "complete reply must produce no truncation envelope"
    );

    let resp = Response {
        id: "comp-1".into(),
        success: true,
        data: fixture.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("grep", &resp, &ctx);

    // Output must be byte-identical to original text
    let original_text = fixture["text"].as_str().unwrap();
    assert_eq!(formatted, original_text);
}

#[test]
fn no_nested_envelopes_and_no_per_selector_reason_words() {
    let fixture_names = [
        "cap_100_of_1204.json",
        "walk_truncated_and_cap.json",
        "skipped_foreign_mounts_walk.json",
        "display_thinned_25_of_42.json",
        "complete_untruncated.json",
    ];

    for name in fixture_names {
        let fixture = load_fixture(name);
        assert_no_bare_list_envelope_key_recursive(&fixture);

        if let Some(env) = build_grep_envelope(&fixture) {
            // Outer unit is always Rows
            assert_eq!(env.unit, Unit::Rows, "unit must be rows in {name}");

            // Reason must only be Cap or Walk, never a selector-specific reason
            let reason = env.reason.expect("reason present");
            assert!(
                matches!(reason, Reason::Cap | Reason::Walk),
                "reason must be Cap or Walk, found {reason:?} in {name}"
            );
            for cause in &env.causes {
                assert!(
                    matches!(cause, Reason::Cap | Reason::Walk),
                    "cause must be Cap or Walk, found {cause:?} in {name}"
                );
            }
        }
    }
}

#[test]
fn handle_grep_attaches_envelope_when_capped() {
    let project = tempfile::tempdir().expect("tempdir");

    // Create 10 files with 15 matches each = 150 matches total
    for i in 0..10 {
        let path = project.path().join(format!("file_{i}.txt"));
        let mut content = String::new();
        for line in 1..=15 {
            content.push_str(&format!("target_needle line {line}\n"));
        }
        fs::write(&path, content).expect("write file");
    }

    let config = aft::config::Config {
        project_root: Some(project.path().to_path_buf()),
        ..aft::config::Config::default()
    };
    let ctx = aft::context::AppContext::from_app(
        aft::context::App::default_shared(),
        crate::context_storage::isolate(config),
    );

    let req = aft::protocol::RawRequest {
        id: "grep-live-1".into(),
        command: "grep".into(),
        lsp_hints: None,
        session_id: None,
        params: serde_json::json!({
            "pattern": "target_needle",
            "max_results": 100,
        }),
    };

    let response = aft::commands::grep::handle_grep(&req, &ctx);
    assert!(response.success);

    let envelope_val = response.data.get("matches_list_envelope");
    assert!(
        envelope_val.is_some(),
        "handle_grep must attach matches_list_envelope when capped"
    );

    let envelope: ListEnvelope =
        serde_json::from_value(envelope_val.unwrap().clone()).expect("valid ListEnvelope");
    assert_eq!(envelope.unit, Unit::Rows);
    assert_eq!(envelope.reason, Some(Reason::Cap));
    assert!(envelope.total.is_at_least());
    assert_eq!(
        envelope.narrow,
        vec!["offset", "path", "include", "exclude"]
    );
}
