use std::fs;
use std::path::Path;

use aft::commands::glob::handle_glob;
use aft::config::Config;
use aft::context::AppContext;
use aft::list_envelope::{derive_wire_key, render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::glob::{
    build_glob_envelope, DEFAULT_MAX_RESULTS, GLOB_LIST_ID, GLOB_NARROW, GLOB_UNIT,
    MAX_DISPLAY_DIRECTORIES, MAX_DISPLAY_FILES_PER_DIRECTORY,
};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::subc_format::{format_response_with_context, report_rendered_row_count, FormatContext};
use serde_json::{json, Value};

fn glob_request(params: Value) -> RawRequest {
    RawRequest {
        id: "glob-test".to_string(),
        command: "glob".to_string(),
        params,
        lsp_hints: None,
        session_id: None,
    }
}

fn test_context(project_root: &Path) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(project_root.to_path_buf()),
            ..Config::default()
        }),
    )
}

fn assert_no_bare_list_envelope_key(val: &Value) {
    match val {
        Value::Object(map) => {
            assert!(
                !map.contains_key("list_envelope"),
                "found forbidden bare 'list_envelope' key"
            );
            for (k, v) in map {
                if k.ends_with("_list_envelope") {
                    assert_eq!(
                        k, "files_list_envelope",
                        "envelope key must be derived from payload.files"
                    );
                }
                assert_no_bare_list_envelope_key(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                assert_no_bare_list_envelope_key(v);
            }
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------------
// Fixture Tests
// -----------------------------------------------------------------------------

#[test]
fn executor_cap_fixture_renders_cap_trailer_and_envelope() {
    let fixture_str = include_str!("fixtures/glob/executor_cap.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    let resp = Response {
        id: "cap-1".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    // Trailer format: shown <n> of ≥<count> files (cap) · narrow: path
    assert!(
        rendered.contains("shown 5 of ≥105 files (cap) · narrow: path"),
        "rendered text must contain cap trailer, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("(Results are truncated:"),
        "legacy truncation clause must be suppressed when envelope is present"
    );

    let envelope_val = &data["files_list_envelope"];
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "cap");
    assert_eq!(envelope_val["causes"], json!(["cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));
    assert_eq!(envelope_val["shown"], 5);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "at_least", "value": 105 })
    );

    // Retained flags in JSON
    assert_eq!(data["truncated"], true);
    assert_eq!(data["complete"], true);
    assert_eq!(data["skipped_foreign_mounts"], 0);

    assert_no_bare_list_envelope_key(&data);
}

#[test]
fn walk_truncation_plus_cap_renders_one_walk_trailer_with_both_causes() {
    let fixture_str = include_str!("fixtures/glob/walk_and_cap.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    let resp = Response {
        id: "walk-cap-1".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    // Walk truncation plus cap renders one (walk) trailer with causes: ["walk","cap"]
    assert!(
        rendered.contains("shown 5 of ≥105 files (walk) · narrow: path"),
        "walk+cap must render (walk) trailer due to Reason::Walk precedence over Cap, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("(Results are truncated:"),
        "legacy truncation message must be suppressed"
    );

    let envelope_val = &data["files_list_envelope"];
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "walk");
    assert_eq!(envelope_val["causes"], json!(["walk", "cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));
    assert_eq!(envelope_val["shown"], 5);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "at_least", "value": 105 })
    );

    // Both flags retained in JSON
    assert_eq!(data["walk_truncated"], true);
    assert_eq!(data["truncated"], true);
    assert_eq!(data["complete"], false);

    assert_no_bare_list_envelope_key(&data);
}

#[test]
fn r25_skipped_foreign_mounts_alone_renders_walk_trailer() {
    let fixture_str = include_str!("fixtures/glob/skipped_foreign_mounts_r25.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    let resp = Response {
        id: "r25-mounts".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    // Test that skipped_foreign_mounts alone causes a walk trailer: shown 12 of ≥12 files (walk) · narrow: path with causes: ["walk"]
    assert!(
        rendered.contains("shown 12 of ≥12 files (walk) · narrow: path"),
        "skipped_foreign_mounts alone must render shown 12 of ≥12 files (walk) · narrow: path, got:\n{rendered}"
    );

    let envelope_val = &data["files_list_envelope"];
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "walk");
    assert_eq!(envelope_val["causes"], json!(["walk"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));
    assert_eq!(envelope_val["shown"], 12);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "at_least", "value": 12 })
    );

    // Asserted independently of walk_truncated fixture:
    assert!(
        data.get("walk_truncated").is_none() || data["walk_truncated"] == false,
        "walk_truncated must NOT be set in this fixture"
    );
    assert_eq!(
        data["skipped_foreign_mounts"], 1,
        "skipped_foreign_mounts must be retained in JSON"
    );
    assert_eq!(data["complete"], false);

    assert_no_bare_list_envelope_key(&data);
}

#[test]
fn walk_truncated_alone_renders_walk_trailer() {
    let fixture_str = include_str!("fixtures/glob/walk_truncated.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    let resp = Response {
        id: "walk-only".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    assert!(
        rendered.contains("shown 12 of ≥12 files (walk) · narrow: path"),
        "walk_truncated alone must render walk trailer, got:\n{rendered}"
    );

    let envelope_val = &data["files_list_envelope"];
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "walk");
    assert_eq!(envelope_val["causes"], json!(["walk"]));
    assert_eq!(envelope_val["shown"], 12);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "at_least", "value": 12 })
    );

    assert_eq!(data["walk_truncated"], true);
    assert_eq!(data["skipped_foreign_mounts"], 0);
    assert_eq!(data["complete"], false);

    assert_no_bare_list_envelope_key(&data);
}

#[test]
fn r24_display_selector_above_max_display_files_per_directory() {
    let fixture_str = include_str!("fixtures/glob/display_files_per_dir_r24.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    let files_len = data["files"].as_array().unwrap().len();
    assert_eq!(files_len, 21);
    assert!(files_len < DEFAULT_MAX_RESULTS);

    // Seam reports rendered count
    let rendered_count = report_rendered_row_count("glob", &data).expect("seam reports glob count");
    assert_eq!(rendered_count, 5);

    let resp = Response {
        id: "r24-files-per-dir".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    // Render: shown 5 of 21 files (cap) · narrow: path with Exact
    assert!(
        rendered.contains("shown 5 of 21 files (cap) · narrow: path"),
        "must render exact total with rendered count 5, got:\n{rendered}"
    );

    let envelope_val = &data["files_list_envelope"];
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "cap");
    assert_eq!(envelope_val["causes"], json!(["cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));
    assert_eq!(envelope_val["shown"], 5);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "exact", "value": 21 })
    );

    assert_no_bare_list_envelope_key(&data);
}

#[test]
fn r24_display_selector_above_max_display_directories() {
    let fixture_str = include_str!("fixtures/glob/display_directories_r24.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    let files_len = data["files"].as_array().unwrap().len();
    assert_eq!(files_len, 21);
    assert!(files_len < DEFAULT_MAX_RESULTS);

    // Seam reports rendered count: 6 displayed dirs * 3 files = 18
    let rendered_count = report_rendered_row_count("glob", &data).expect("seam reports glob count");
    assert_eq!(rendered_count, 18);

    let resp = Response {
        id: "r24-dirs".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    // Render: shown 18 of 21 files (cap) · narrow: path with Exact
    assert!(
        rendered.contains("shown 18 of 21 files (cap) · narrow: path"),
        "must render exact total with rendered count 18, got:\n{rendered}"
    );

    let envelope_val = &data["files_list_envelope"];
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "cap");
    assert_eq!(envelope_val["causes"], json!(["cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));
    assert_eq!(envelope_val["shown"], 18);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "exact", "value": 21 })
    );

    assert_no_bare_list_envelope_key(&data);
}

#[test]
fn complete_glob_replies_byte_identical_to_prespec_goldens() {
    let fixture_str = include_str!("fixtures/glob/complete.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");

    // Complete reply has NO envelope
    assert!(
        data.get("files_list_envelope").is_none(),
        "complete reply must serialize no envelope"
    );

    let resp = Response {
        id: "complete-1".to_string(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &ctx);

    let expected_golden = data["text"].as_str().unwrap();
    assert_eq!(
        rendered, expected_golden,
        "complete glob replies must be byte-identical to pre-spec goldens"
    );
    assert!(
        !rendered.contains("shown "),
        "complete reply must render no trailer"
    );

    assert_no_bare_list_envelope_key(&data);
}

// -----------------------------------------------------------------------------
// Mutation Test for JSON Array Length vs Rendered Count
// -----------------------------------------------------------------------------

#[test]
fn mutation_shown_sourced_from_json_array_length_reds() {
    // Verify that using the raw JSON array length instead of the seam's rendered-row count produces a mutant that fails test expectations.

    // Control 1: display_files_per_dir
    let fixture_str = include_str!("fixtures/glob/display_files_per_dir_r24.json");
    let data: Value = serde_json::from_str(fixture_str).expect("valid fixture");
    let seam_rendered = report_rendered_row_count("glob", &data).unwrap();
    let array_length = data["files"].as_array().unwrap().len();
    assert_eq!(seam_rendered, 5);
    assert_eq!(array_length, 21);

    // Mutated envelope using array length as `shown`
    let mutated_env = ListEnvelope::new(
        array_length, // <-- MUTATION: using JSON array length instead of seam count
        Total::Exact(array_length),
        Unit::Files,
        vec![Reason::Cap],
        &["path"],
    );
    let mutated_trailer = render_trailer(&mutated_env).unwrap();
    assert_eq!(mutated_trailer, "shown 21 of 21 files (cap) · narrow: path");
    assert_ne!(
        mutated_trailer, "shown 5 of 21 files (cap) · narrow: path",
        "mutation must produce different trailer text that fails the contract"
    );

    // Control 2: display_directories
    let fixture_str2 = include_str!("fixtures/glob/display_directories_r24.json");
    let data2: Value = serde_json::from_str(fixture_str2).expect("valid fixture");
    let seam_rendered2 = report_rendered_row_count("glob", &data2).unwrap();
    let array_length2 = data2["files"].as_array().unwrap().len();
    assert_eq!(seam_rendered2, 18);
    assert_eq!(array_length2, 21);

    let mutated_env2 = ListEnvelope::new(
        array_length2, // <-- MUTATION
        Total::Exact(array_length2),
        Unit::Files,
        vec![Reason::Cap],
        &["path"],
    );
    let mutated_trailer2 = render_trailer(&mutated_env2).unwrap();
    assert_eq!(
        mutated_trailer2,
        "shown 21 of 21 files (cap) · narrow: path"
    );
    assert_ne!(
        mutated_trailer2, "shown 18 of 21 files (cap) · narrow: path",
        "mutation must produce different trailer text that fails the contract"
    );
}

// -----------------------------------------------------------------------------
// End-to-End Tests with handle_glob
// -----------------------------------------------------------------------------

#[test]
fn end_to_end_glob_complete_query_produces_no_envelope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    fs::write(root.join("a.txt"), "hello").unwrap();
    fs::write(root.join("b.txt"), "world").unwrap();
    fs::write(root.join("c.txt"), "!").unwrap();

    let ctx = test_context(root);
    let req = glob_request(json!({ "pattern": "*.txt" }));

    let resp = handle_glob(&req, &ctx);
    assert!(resp.success);
    assert!(
        resp.data.get("files_list_envelope").is_none(),
        "complete query must have no envelope"
    );

    let fmt_ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &fmt_ctx);
    assert!(
        !rendered.contains("shown "),
        "complete query must render no trailer"
    );
    assert_eq!(rendered, resp.data["text"].as_str().unwrap());

    assert_no_bare_list_envelope_key(&resp.data);
}

#[test]
fn end_to_end_glob_display_files_per_directory_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let dir1 = root.join("dir1");
    fs::create_dir_all(&dir1).unwrap();

    // Create 21 files in dir1 to exceed the per-directory display limit of 5 files
    for i in 1..=21 {
        fs::write(dir1.join(format!("file_{i:02}.txt")), "data").unwrap();
    }

    let ctx = test_context(root);
    let req = glob_request(json!({ "pattern": "**/*.txt" }));

    let resp = handle_glob(&req, &ctx);
    assert!(resp.success);

    let envelope_val = resp
        .data
        .get("files_list_envelope")
        .expect("envelope must be present");
    assert_eq!(envelope_val["shown"], 5);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "exact", "value": 21 })
    );
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "cap");
    assert_eq!(envelope_val["causes"], json!(["cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));

    let fmt_ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &fmt_ctx);
    assert!(
        rendered.contains("shown 5 of 21 files (cap) · narrow: path"),
        "rendered text must contain cap trailer with exact total, got:\n{rendered}"
    );

    assert_no_bare_list_envelope_key(&resp.data);
}

#[test]
fn end_to_end_glob_display_directories_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Create 7 directories exceeding the maximum display limit of 6 directories, each with 3 files (21 files total)
    for d in 1..=7 {
        let dir = root.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 1..=3 {
            fs::write(dir.join(format!("file_{f}.txt")), "data").unwrap();
        }
    }

    let ctx = test_context(root);
    let req = glob_request(json!({ "pattern": "**/*.txt" }));

    let resp = handle_glob(&req, &ctx);
    assert!(resp.success);

    let envelope_val = resp
        .data
        .get("files_list_envelope")
        .expect("envelope must be present");
    assert_eq!(envelope_val["shown"], 18);
    assert_eq!(
        envelope_val["total"],
        json!({ "kind": "exact", "value": 21 })
    );
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "cap");
    assert_eq!(envelope_val["causes"], json!(["cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));

    let fmt_ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &fmt_ctx);
    assert!(
        rendered.contains("shown 18 of 21 files (cap) · narrow: path"),
        "rendered text must contain cap trailer with exact total, got:\n{rendered}"
    );

    assert_no_bare_list_envelope_key(&resp.data);
}

#[test]
fn end_to_end_glob_executor_cap_renders_at_least_trailer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let dir = root.join("large_dir");
    fs::create_dir_all(&dir).unwrap();

    // Create 105 files (> DEFAULT_MAX_RESULTS 100)
    for i in 1..=105 {
        fs::write(dir.join(format!("file_{i:03}.txt")), "content").unwrap();
    }

    let ctx = test_context(root);
    let req = glob_request(json!({ "pattern": "**/*.txt" }));

    let resp = handle_glob(&req, &ctx);
    assert!(resp.success);
    assert_eq!(resp.data["truncated"], true);

    let envelope_val = resp
        .data
        .get("files_list_envelope")
        .expect("envelope must be present");
    assert_eq!(envelope_val["shown"], 5); // 1 dir, capped at 5 files per dir
    assert_eq!(envelope_val["unit"], "files");
    assert_eq!(envelope_val["reason"], "cap");
    assert_eq!(envelope_val["causes"], json!(["cap"]));
    assert_eq!(envelope_val["narrow"], json!(["path"]));

    // Total must be AtLeast(total)
    let total_kind = envelope_val["total"]["kind"].as_str().unwrap();
    assert_eq!(total_kind, "at_least");

    let fmt_ctx = FormatContext::default();
    let rendered = format_response_with_context("glob", &resp, &fmt_ctx);
    assert!(
        rendered.contains("shown 5 of ≥"),
        "rendered text must contain shown 5 of ≥..., got:\n{rendered}"
    );
    assert!(
        rendered.contains("files (cap) · narrow: path"),
        "rendered text must contain trailer reason (cap) and narrow: path, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("(Results are truncated:"),
        "legacy truncation message must be suppressed"
    );

    assert_no_bare_list_envelope_key(&resp.data);
}

// -----------------------------------------------------------------------------
// Surface Registration Contract
// -----------------------------------------------------------------------------

#[test]
fn surface_registration_constants_and_types() {
    assert_eq!(GLOB_LIST_ID, "payload.files");
    assert_eq!(GLOB_UNIT, Unit::Files);
    assert_eq!(GLOB_NARROW, &["path"]);
    assert_eq!(DEFAULT_MAX_RESULTS, 100);
    assert_eq!(MAX_DISPLAY_FILES_PER_DIRECTORY, 5);
    assert_eq!(MAX_DISPLAY_DIRECTORIES, 6);

    let wire_key = derive_wire_key(GLOB_LIST_ID, false);
    assert_eq!(wire_key, "files_list_envelope");

    // Complete query returns None
    let env_complete = build_glob_envelope(5, 5, false, false, 0);
    assert!(env_complete.is_none());

    // Cap alone (display thinning)
    let env_cap = build_glob_envelope(5, 21, false, false, 0).unwrap();
    assert_eq!(env_cap.shown, 5);
    assert_eq!(env_cap.total, Total::Exact(21));
    assert_eq!(env_cap.reason, Some(Reason::Cap));
    assert_eq!(env_cap.causes, vec![Reason::Cap]);

    // Cap alone (executor cap)
    let env_exec_cap = build_glob_envelope(5, 105, true, false, 0).unwrap();
    assert_eq!(env_exec_cap.shown, 5);
    assert_eq!(env_exec_cap.total, Total::AtLeast(105));
    assert_eq!(env_exec_cap.reason, Some(Reason::Cap));
    assert_eq!(env_exec_cap.causes, vec![Reason::Cap]);

    // Walk alone (walk_truncated)
    let env_walk = build_glob_envelope(12, 12, false, true, 0).unwrap();
    assert_eq!(env_walk.shown, 12);
    assert_eq!(env_walk.total, Total::AtLeast(12));
    assert_eq!(env_walk.reason, Some(Reason::Walk));
    assert_eq!(env_walk.causes, vec![Reason::Walk]);

    // Walk alone (skipped_foreign_mounts)
    let env_mounts = build_glob_envelope(12, 12, false, false, 1).unwrap();
    assert_eq!(env_mounts.shown, 12);
    assert_eq!(env_mounts.total, Total::AtLeast(12));
    assert_eq!(env_mounts.reason, Some(Reason::Walk));
    assert_eq!(env_mounts.causes, vec![Reason::Walk]);

    // Walk + Cap
    let env_both = build_glob_envelope(5, 105, true, true, 0).unwrap();
    assert_eq!(env_both.shown, 5);
    assert_eq!(env_both.total, Total::AtLeast(105));
    assert_eq!(env_both.reason, Some(Reason::Walk));
    assert_eq!(env_both.causes, vec![Reason::Walk, Reason::Cap]);
}
