use aft::list_envelope::{ListEnvelope, Reason, Total, Unit};
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{
    format_response_with_context, rendered_glob_file_count, rendered_grep_match_count,
    report_rendered_row_count, FormatContext,
};
use serde_json::json;

#[test]
fn seam_grep_suppresses_legacy_clause_and_renders_trailer_when_envelope_present() {
    let pre_spec_text = "src/main.rs:10: fn run()\n\nFound 100 match across 1 file (capped)";
    let mut data = json!({
        "text": pre_spec_text,
        "matches": [{ "file": "src/main.rs", "line": 10, "line_text": "fn run()" }],
        "total_matches": 100,
        "files_with_matches": 1,
    });

    let resp_absent = Response {
        id: "1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();

    // Absent: pre-spec text is byte-unchanged
    let formatted_absent = format_response_with_context("grep", &resp_absent, &ctx);
    assert_eq!(formatted_absent, pre_spec_text);

    // Present: (capped) is suppressed, trailer rendered
    let envelope = ListEnvelope::new(
        100,
        Total::AtLeast(100),
        Unit::Rows,
        vec![Reason::Cap],
        &["path", "include", "exclude"],
    );
    data["matches_list_envelope"] = serde_json::to_value(&envelope).unwrap();
    let resp_present = Response {
        id: "2".into(),
        success: true,
        data,
    };
    let formatted_present = format_response_with_context("grep", &resp_present, &ctx);
    assert!(!formatted_present.contains("(capped)"));
    assert!(
        formatted_present.contains("shown 100 of ≥100 rows (cap) · narrow: path, include, exclude")
    );
}

#[test]
fn seam_glob_suppresses_legacy_clause_and_renders_trailer_when_envelope_present() {
    let pre_spec_text = "100 files matching **/*\n\nsrc/a.rs\n\n(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)";
    let mut data = json!({
        "text": pre_spec_text,
        "files": ["src/a.rs"],
    });

    let resp_absent = Response {
        id: "1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();

    // Absent: pre-spec text is byte-unchanged
    let formatted_absent = format_response_with_context("glob", &resp_absent, &ctx);
    assert_eq!(formatted_absent, pre_spec_text);

    // Present: truncated message is suppressed, trailer rendered
    let envelope = ListEnvelope::new(
        100,
        Total::AtLeast(100),
        Unit::Files,
        vec![Reason::Cap],
        &["path"],
    );
    data["files_list_envelope"] = serde_json::to_value(&envelope).unwrap();
    let resp_present = Response {
        id: "2".into(),
        success: true,
        data,
    };
    let formatted_present = format_response_with_context("glob", &resp_present, &ctx);
    assert!(!formatted_present.contains("Results are truncated"));
    assert!(formatted_present.contains("shown 100 of ≥100 files (cap) · narrow: path"));
}

#[test]
fn seam_search_suppresses_legacy_clauses_and_renders_trailer_when_envelope_present() {
    let mut data = json!({
        "more_available": true,
        "engine_capped": true,
        "fully_degraded": true,
        "complete": false,
        "results": [],
    });

    let resp_absent = Response {
        id: "1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();

    // Absent: pre-spec text contains legacy notes
    let formatted_absent = format_response_with_context("search", &resp_absent, &ctx);
    assert!(formatted_absent.contains("more results available"));
    assert!(formatted_absent.contains("enumeration capped"));

    // Present: more_available and engine_capped suppressed from text, trailer rendered, status text preserved
    let envelope = ListEnvelope::new(
        10,
        Total::AtLeast(11),
        Unit::Results,
        vec![Reason::Budget, Reason::Cap],
        &["offset", "topK", "path", "includeTests"],
    );
    data["results_list_envelope"] = serde_json::to_value(&envelope).unwrap();
    let resp_present = Response {
        id: "2".into(),
        success: true,
        data,
    };
    let formatted_present = format_response_with_context("search", &resp_present, &ctx);
    assert!(!formatted_present.contains("more results available"));
    assert!(!formatted_present.contains("enumeration capped"));
    assert!(formatted_present.contains("fully degraded"));
    assert!(formatted_present.contains("partial/incomplete"));
    assert!(formatted_present
        .contains("shown 10 of ≥11 results (budget) · narrow: offset, topK, path, includeTests"));
}

#[test]
fn seam_bash_passes_through_unchanged_so_no_trailer_renders_twice() {
    let raw_bash_output = "line 1\nline 2\nshown 2 of 100 lines (cap)";
    let data = json!({
        "output": raw_bash_output,
        "bash_output_list_envelope": {
            "shown": 2,
            "total": { "kind": "exact", "value": 100 },
            "unit": "lines",
            "reason": "cap",
            "causes": ["cap"],
            "narrow": []
        }
    });

    let resp = Response {
        id: "bash-1".into(),
        success: true,
        data,
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context("bash", &resp, &ctx);

    // subc formatter passes bash text through unchanged: no second trailer appended
    assert_eq!(formatted, raw_bash_output);
    assert_eq!(formatted.matches("shown 2 of 100 lines (cap)").count(), 1);
}

#[test]
fn seam_reports_rendered_row_counts_back_to_producers() {
    // Grep display selector: MAX_DISPLAY_MATCHES_PER_FILE = 5
    let grep_data = json!({
        "matches": [
            { "file": "src/a.rs", "line": 1 },
            { "file": "src/a.rs", "line": 2 },
            { "file": "src/a.rs", "line": 3 },
            { "file": "src/a.rs", "line": 4 },
            { "file": "src/a.rs", "line": 5 },
            { "file": "src/a.rs", "line": 6 },
            { "file": "src/a.rs", "line": 7 },
            { "file": "src/b.rs", "line": 1 },
            { "file": "src/b.rs", "line": 2 },
        ]
    });
    // 7 matches in a.rs (capped to 5) + 2 matches in b.rs = 7 rendered rows
    assert_eq!(rendered_grep_match_count(&grep_data), 7);
    assert_eq!(report_rendered_row_count("grep", &grep_data), Some(7));

    // Glob display selector
    let mut files = Vec::new();
    for i in 1..=10 {
        files.push(format!("dir1/file{i}.rs"));
    }
    for i in 1..=10 {
        files.push(format!("dir2/file{i}.rs"));
    }
    let glob_data = json!({ "files": files });
    // 20 files -> <= 20 flat files -> 20 rendered rows
    assert_eq!(rendered_glob_file_count(&glob_data), 20);

    // >20 files: 8 files in dir1, 8 files in dir2, 8 files in dir3 = 24 files
    let mut files_large = Vec::new();
    for dir in ["d1", "d2", "d3", "d4", "d5", "d6", "d7"] {
        for i in 1..=8 {
            files_large.push(format!("{dir}/file{i}.rs"));
        }
    }
    let glob_large_data = json!({ "files": files_large });
    // 7 directories (> MAX_DISPLAY_DIRECTORIES 6), each with 8 files (> MAX_DISPLAY_FILES_PER_DIRECTORY 5)
    // 6 displayed directories * 5 displayed files = 30 rendered rows
    assert_eq!(rendered_glob_file_count(&glob_large_data), 30);
    assert_eq!(
        report_rendered_row_count("glob", &glob_large_data),
        Some(30)
    );
}

#[test]
fn ndjson_text_builder_renders_trailer_when_envelope_present() {
    let envelope = ListEnvelope::new(
        15,
        Total::Exact(412),
        Unit::Sites,
        vec![Reason::Cap],
        &["depth", "includeTests"],
    );
    let data = json!({
        "payload": {
            "sites_list_envelope": envelope,
        }
    });

    let built = build_ndjson_text("base text", &data, Some("payload.sites"), false);
    assert_eq!(
        built,
        "base text\n\nshown 15 of 412 sites (cap) · narrow: depth, includeTests"
    );

    // Absent: returns base text unchanged
    let empty_data = json!({});
    let unchanged = build_ndjson_text("base text", &empty_data, Some("payload.sites"), false);
    assert_eq!(unchanged, "base text");
}
